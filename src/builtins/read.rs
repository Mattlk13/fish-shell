//! Implementation of the read builtin.

use super::prelude::*;
use crate::common::escape;
use crate::common::read_blocked;
use crate::common::str2wcstring;
use crate::common::unescape_string;
use crate::common::valid_var_name;
use crate::common::UnescapeStringStyle;
use crate::env::EnvMode;
use crate::env::Environment;
use crate::env::READ_BYTE_LIMIT;
use crate::env::{EnvVar, EnvVarFlags};
use crate::input_common::decode_input_byte;
use crate::input_common::DecodeState;
use crate::input_common::InvalidPolicy;
use crate::nix::isatty;
use crate::reader::commandline_set_buffer;
use crate::reader::reader_save_screen_state;
use crate::reader::ReaderConfig;
use crate::reader::{reader_pop, reader_push, reader_readline};
use crate::tokenizer::Tokenizer;
use crate::tokenizer::TOK_ACCEPT_UNFINISHED;
use crate::tokenizer::TOK_ARGUMENT_LIST;
use crate::tty_handoff::TtyHandoff;
use crate::wcstringutil::split_about;
use crate::wcstringutil::split_string_tok;
use crate::wutil;
use crate::wutil::encoding::zero_mbstate;
use crate::wutil::perror;
use libc::SEEK_CUR;
use std::num::NonZeroUsize;
use std::os::fd::RawFd;
use std::sync::atomic::Ordering;

#[derive(Default)]
struct Options {
    print_help: bool,
    place: EnvMode,
    prompt: Option<WString>,
    prompt_str: Option<WString>,
    right_prompt: WString,
    commandline: Option<WString>,
    // If a delimiter was given. Used to distinguish between the default
    // empty string and a given empty delimiter.
    delimiter: Option<WString>,
    tokenize: bool,
    shell: bool,
    array: bool,
    silent: bool,
    split_null: bool,
    to_stdout: bool,
    nchars: usize,
    one_line: bool,
}

impl Options {
    fn new() -> Self {
        Options {
            place: EnvMode::USER,
            ..Default::default()
        }
    }
}

const SHORT_OPTIONS: &wstr = L!(":ac:d:fghiLln:p:sStuxzP:UR:L");
const LONG_OPTIONS: &[WOption] = &[
    wopt(L!("array"), ArgType::NoArgument, 'a'),
    wopt(L!("command"), ArgType::RequiredArgument, 'c'),
    wopt(L!("delimiter"), ArgType::RequiredArgument, 'd'),
    wopt(L!("export"), ArgType::NoArgument, 'x'),
    wopt(L!("function"), ArgType::NoArgument, 'f'),
    wopt(L!("global"), ArgType::NoArgument, 'g'),
    wopt(L!("help"), ArgType::NoArgument, 'h'),
    wopt(L!("line"), ArgType::NoArgument, 'L'),
    wopt(L!("list"), ArgType::NoArgument, 'a'),
    wopt(L!("local"), ArgType::NoArgument, 'l'),
    wopt(L!("nchars"), ArgType::RequiredArgument, 'n'),
    wopt(L!("null"), ArgType::NoArgument, 'z'),
    wopt(L!("prompt"), ArgType::RequiredArgument, 'p'),
    wopt(L!("prompt-str"), ArgType::RequiredArgument, 'P'),
    wopt(L!("right-prompt"), ArgType::RequiredArgument, 'R'),
    wopt(L!("shell"), ArgType::NoArgument, 'S'),
    wopt(L!("silent"), ArgType::NoArgument, 's'),
    wopt(L!("tokenize"), ArgType::NoArgument, 't'),
    wopt(L!("unexport"), ArgType::NoArgument, 'u'),
    wopt(L!("universal"), ArgType::NoArgument, 'U'),
];

fn parse_cmd_opts(
    args: &mut [&wstr],
    parser: &Parser,
    streams: &mut IoStreams,
) -> Result<(Options, usize), ErrorCode> {
    let cmd = args[0];
    let mut opts = Options::new();
    let mut w = WGetopter::new(SHORT_OPTIONS, LONG_OPTIONS, args);
    while let Some(opt) = w.next_opt() {
        match opt {
            'a' => {
                opts.array = true;
            }
            'c' => {
                opts.commandline = Some(w.woptarg.unwrap().to_owned());
            }
            'd' => {
                opts.delimiter = Some(w.woptarg.unwrap().to_owned());
            }
            'i' => {
                streams.err.append(wgettext_fmt!(
                    "%ls: usage of -i for --silent is deprecated. Please use -s or --silent instead.\n",
                    cmd
                ));
                return Err(STATUS_INVALID_ARGS);
            }
            'f' => {
                opts.place |= EnvMode::FUNCTION;
            }
            'g' => {
                opts.place |= EnvMode::GLOBAL;
            }
            'h' => {
                opts.print_help = true;
            }
            'L' => {
                opts.one_line = true;
            }
            'l' => {
                opts.place |= EnvMode::LOCAL;
            }
            'n' => {
                opts.nchars = match fish_wcstoi(w.woptarg.unwrap()) {
                    Ok(n) if n >= 0 => n.try_into().unwrap(),
                    Err(wutil::Error::Overflow) => {
                        streams.err.append(wgettext_fmt!(
                            "%ls: Argument '%ls' is out of range\n",
                            cmd,
                            w.woptarg.unwrap()
                        ));
                        builtin_print_error_trailer(parser, streams.err, cmd);
                        return Err(STATUS_INVALID_ARGS);
                    }
                    _ => {
                        streams.err.append(wgettext_fmt!(
                            BUILTIN_ERR_NOT_NUMBER,
                            cmd,
                            w.woptarg.unwrap()
                        ));
                        builtin_print_error_trailer(parser, streams.err, cmd);
                        return Err(STATUS_INVALID_ARGS);
                    }
                }
            }
            'P' => {
                opts.prompt_str = Some(w.woptarg.unwrap().to_owned());
            }
            'p' => {
                opts.prompt = Some(w.woptarg.unwrap().to_owned());
            }
            'R' => {
                opts.right_prompt = w.woptarg.unwrap().to_owned();
            }
            's' => {
                opts.silent = true;
            }
            'S' => {
                opts.shell = true;
            }
            't' => {
                opts.tokenize = true;
            }
            'U' => {
                opts.place |= EnvMode::UNIVERSAL;
            }
            'u' => {
                opts.place |= EnvMode::UNEXPORT;
            }
            'x' => {
                opts.place |= EnvMode::EXPORT;
            }
            'z' => {
                opts.split_null = true;
            }
            ':' => {
                builtin_missing_argument(parser, streams, cmd, args[w.wopt_index - 1], true);
                return Err(STATUS_INVALID_ARGS);
            }
            '?' => {
                builtin_unknown_option(parser, streams, cmd, args[w.wopt_index - 1], true);
                return Err(STATUS_INVALID_ARGS);
            }
            _ => {
                panic!("unexpected retval from WGetopter");
            }
        }
    }

    Ok((opts, w.wopt_index))
}

/// Read from the tty. This is only valid when the stream is stdin and it is attached to a tty and
/// we weren't asked to split on null characters.
fn read_interactive(
    parser: &Parser,
    buff: &mut WString,
    nchars: usize,
    shell: bool,
    silent: bool,
    prompt: &wstr,
    right_prompt: &wstr,
    commandline: &Option<WString>,
    inputfd: RawFd,
) -> BuiltinResult {
    let mut exit_res = Ok(SUCCESS);

    // Construct a configuration.
    let mut conf = ReaderConfig::default();
    conf.complete_ok = shell;
    conf.highlight_ok = shell;
    conf.syntax_check_ok = shell;

    // No autosuggestions or abbreviations in builtin_read.
    conf.autosuggest_ok = false;
    conf.expand_abbrev_ok = false;

    conf.exit_on_interrupt = true;
    conf.in_silent_mode = silent;

    conf.left_prompt_cmd = prompt.to_owned();
    conf.right_prompt_cmd = right_prompt.to_owned();
    conf.event = L!("fish_read");

    conf.inputfd = inputfd;

    // Keep in-memory history only.
    reader_push(parser, L!(""), conf);
    let _modifiable_commandline = parser.scope().readonly_commandline.then(|| {
        parser.push_scope(|s| {
            s.readonly_commandline = false;
        })
    });
    if let Some(commandline) = commandline {
        commandline_set_buffer(parser, Some(commandline.clone()), None);
    }

    let mline = {
        let _interactive = parser.push_scope(|s| s.is_interactive = true);
        let mut scoped_handoff = TtyHandoff::new(reader_save_screen_state);
        scoped_handoff.enable_tty_protocols();
        reader_readline(parser, NonZeroUsize::try_from(nchars).ok())
    };
    if let Some(line) = mline {
        *buff = line;
        if nchars > 0 && nchars < buff.len() {
            // Line may be longer than nchars if a keybinding used `commandline -i`
            // note: we're deliberately throwing away the tail of the commandline.
            // It shouldn't be unread because it was produced with `commandline -i`,
            // not typed.
            buff.truncate(nchars);
        }
    } else {
        exit_res = Err(STATUS_CMD_ERROR);
    }
    reader_pop();
    exit_res
}

/// Bash uses 128 bytes for its chunk size. Very informal testing I did suggested that a smaller
/// chunk size performed better. However, we're going to use the bash value under the assumption
/// they've done more extensive testing.
const READ_CHUNK_SIZE: usize = 128;

/// Read from the fd in chunks until we see newline or null, as requested, is seen. This is only
/// used when the fd is seekable (so not from a tty or pipe) and we're not reading a specific number
/// of chars.
///
/// Returns an exit status.
fn read_in_chunks(fd: RawFd, buff: &mut WString, split_null: bool, do_seek: bool) -> BuiltinResult {
    let mut exit_res = Ok(SUCCESS);
    let mut narrow_buff = vec![];
    let mut eof = false;
    let mut finished = false;

    while !finished {
        let mut inbuf = [0_u8; READ_CHUNK_SIZE];

        let bytes_read = match read_blocked(fd, &mut inbuf) {
            Ok(0) | Err(_) => {
                eof = true;
                break;
            }
            Ok(read) => read,
        };

        let bytes_consumed = inbuf[..bytes_read]
            .iter()
            .position(|c| *c == if split_null { b'\0' } else { b'\n' })
            .unwrap_or(bytes_read);
        assert!(bytes_consumed <= bytes_read);
        narrow_buff.extend_from_slice(&inbuf[..bytes_consumed]);
        if bytes_consumed < bytes_read {
            // We found a splitter. The +1 because we need to treat the splitter as consumed, but
            // not append it to the string.
            if do_seek
                && unsafe {
                    libc::lseek(
                        fd,
                        libc::off_t::try_from(
                            isize::try_from(bytes_consumed).unwrap() - (bytes_read as isize) + 1,
                        )
                        .unwrap(),
                        SEEK_CUR,
                    )
                } == -1
            {
                perror("lseek");
                return Err(STATUS_CMD_ERROR);
            }
            finished = true;
        } else if narrow_buff.len() > READ_BYTE_LIMIT.load(Ordering::Relaxed) {
            exit_res = Err(STATUS_READ_TOO_MUCH);
            finished = true;
        }
    }

    *buff = str2wcstring(&narrow_buff);
    if buff.is_empty() && eof {
        exit_res = Err(STATUS_CMD_ERROR);
    }

    exit_res
}

/// Read from the fd on char at a time until we've read the requested number of characters or a
/// newline or null, as appropriate, is seen. This is inefficient so should only be used when the
/// fd is not seekable.
fn read_one_char_at_a_time(
    fd: RawFd,
    buff: &mut WString,
    nchars: usize,
    split_null: bool,
) -> BuiltinResult {
    let mut exit_res = Ok(SUCCESS);
    let mut nbytes = 0;

    let mut unconsumed = vec![];

    loop {
        let mut state = zero_mbstate();

        let chars_read = buff.len();
        let res = loop {
            let mut b = [0_u8; 1];
            match read_blocked(fd, &mut b) {
                Ok(0) | Err(_) => {
                    break None;
                }
                _ => {}
            }
            let b = b[0];
            unconsumed.push(b);
            nbytes += 1;
            let mut consumed = 0;
            match decode_input_byte(
                buff,
                InvalidPolicy::Passthrough,
                &mut state,
                &unconsumed,
                &mut consumed,
            ) {
                DecodeState::Incomplete => continue,
                DecodeState::Complete => {
                    unconsumed.clear();
                    break Some(buff.as_char_slice().last().unwrap());
                }
                DecodeState::Error => unreachable!(),
            }
        };

        if nbytes > READ_BYTE_LIMIT.load(Ordering::Relaxed) {
            // Historical behavior: do not include the codepoint that made us overflow.
            buff.truncate(chars_read);
            exit_res = Err(STATUS_READ_TOO_MUCH);
            break;
        }
        let Some(&res) = res else {
            // EOF
            if buff.is_empty() {
                exit_res = Err(STATUS_CMD_ERROR);
            }
            break;
        };
        if res == if split_null { '\0' } else { '\n' } {
            buff.pop();
            break;
        }
        if nchars > 0 && nchars <= buff.len() {
            break;
        }
    }

    exit_res
}

/// Validate the arguments given to `read` and provide defaults where needed.
fn validate_read_args(
    cmd: &wstr,
    opts: &mut Options,
    argv: &[&wstr],
    parser: &Parser,
    streams: &mut IoStreams,
) -> BuiltinResult {
    if opts.prompt.is_some() && opts.prompt_str.is_some() {
        streams.err.append(wgettext_fmt!(
            "%ls: Options %ls and %ls cannot be used together\n",
            cmd,
            "-p",
            "-P",
        ));
        builtin_print_error_trailer(parser, streams.err, cmd);
        return Err(STATUS_INVALID_ARGS);
    }

    if opts.delimiter.is_some() && opts.one_line {
        streams.err.append(wgettext_fmt!(
            "%ls: Options %ls and %ls cannot be used together\n",
            cmd,
            "--delimiter",
            "--line"
        ));
        return Err(STATUS_INVALID_ARGS);
    }
    if opts.one_line && opts.split_null {
        streams.err.append(wgettext_fmt!(
            "%ls: Options %ls and %ls cannot be used together\n",
            cmd,
            "-z",
            "--line"
        ));
        return Err(STATUS_INVALID_ARGS);
    }

    if let Some(prompt_str) = opts.prompt_str.as_ref() {
        opts.prompt = Some(L!("echo ").to_owned() + &escape(prompt_str)[..]);
    } else if opts.prompt.is_none() {
        opts.prompt = Some(DEFAULT_READ_PROMPT.to_owned());
    }

    if opts.place.contains(EnvMode::UNEXPORT) && opts.place.contains(EnvMode::EXPORT) {
        streams.err.append(wgettext_fmt!(BUILTIN_ERR_EXPUNEXP, cmd));
        builtin_print_error_trailer(parser, streams.err, cmd);
        return Err(STATUS_INVALID_ARGS);
    }

    if opts
        .place
        .intersection(EnvMode::LOCAL | EnvMode::FUNCTION | EnvMode::GLOBAL | EnvMode::UNIVERSAL)
        .iter()
        .count()
        > 1
    {
        streams.err.append(wgettext_fmt!(BUILTIN_ERR_GLOCAL, cmd));
        builtin_print_error_trailer(parser, streams.err, cmd);
        return Err(STATUS_INVALID_ARGS);
    }

    let argc = argv.len();
    if !opts.array && argc < 1 && !opts.to_stdout {
        streams
            .err
            .append(wgettext_fmt!(BUILTIN_ERR_MIN_ARG_COUNT1, cmd, 1, argc));
        return Err(STATUS_INVALID_ARGS);
    }

    if opts.array && argc != 1 {
        streams
            .err
            .append(wgettext_fmt!(BUILTIN_ERR_ARG_COUNT1, cmd, 1, argc));
        return Err(STATUS_INVALID_ARGS);
    }

    if opts.to_stdout && argc > 0 {
        streams
            .err
            .append(wgettext_fmt!(BUILTIN_ERR_MAX_ARG_COUNT1, cmd, 0, argc));
        return Err(STATUS_INVALID_ARGS);
    }

    if opts.tokenize && opts.delimiter.is_some() {
        streams.err.append(wgettext_fmt!(
            BUILTIN_ERR_COMBO2_EXCLUSIVE,
            cmd,
            "--delimiter",
            "--tokenize"
        ));
        return Err(STATUS_INVALID_ARGS);
    }

    if opts.tokenize && opts.one_line {
        streams.err.append(wgettext_fmt!(
            BUILTIN_ERR_COMBO2_EXCLUSIVE,
            cmd,
            "--line",
            "--tokenize"
        ));
        return Err(STATUS_INVALID_ARGS);
    }

    // Verify all variable names.
    for arg in argv {
        if !valid_var_name(arg) {
            streams
                .err
                .append(wgettext_fmt!(BUILTIN_ERR_VARNAME, cmd, arg));
            builtin_print_error_trailer(parser, streams.err, cmd);
            return Err(STATUS_INVALID_ARGS);
        }
        if EnvVar::flags_for(arg).contains(EnvVarFlags::READ_ONLY) {
            streams.err.append(wgettext_fmt!(
                "%ls: %ls: cannot overwrite read-only variable",
                cmd,
                arg
            ));
            builtin_print_error_trailer(parser, streams.err, cmd);
            return Err(STATUS_INVALID_ARGS);
        }
    }

    Ok(SUCCESS)
}

/// The read builtin. Reads from stdin and stores the values in environment variables.
pub fn read(parser: &Parser, streams: &mut IoStreams, argv: &mut [&wstr]) -> BuiltinResult {
    let mut buff = WString::new();
    let mut exit_res: BuiltinResult;

    let (mut opts, optind) = parse_cmd_opts(argv, parser, streams)?;

    let cmd = argv[0];
    let mut argv: &[&wstr] = argv;
    if !opts.to_stdout {
        argv = &argv[optind..];
    }
    let argc = argv.len();

    if argv.is_empty() {
        opts.to_stdout = true;
    }

    if opts.print_help {
        builtin_print_help(parser, streams, cmd);
        return Ok(SUCCESS);
    }

    validate_read_args(cmd, &mut opts, argv, parser, streams)?;

    // stdin may have been explicitly closed
    if streams.stdin_fd < 0 {
        streams
            .err
            .append(wgettext_fmt!("%ls: stdin is closed\n", cmd));
        return Err(STATUS_CMD_ERROR);
    }

    if opts.one_line {
        // --line is the same as read -d \n repeated N times
        opts.delimiter = Some(L!("\n").to_owned());
        opts.split_null = false;
        opts.shell = false;
    }

    let mut var_ptr = 0;
    let vars_left = |var_ptr: usize| argc - var_ptr;
    let clear_remaining_vars = |var_ptr: &mut usize| {
        while vars_left(*var_ptr) != 0 {
            parser.vars().set_empty(argv[*var_ptr], opts.place);
            *var_ptr += 1;
        }
    };

    let stream_stdin_is_a_tty = isatty(streams.stdin_fd);

    // Normally, we either consume a line of input or all available input. But if we are reading a
    // line at a time, we need a middle ground where we only consume as many lines as we need to
    // fill the given vars.
    loop {
        buff.clear();

        if stream_stdin_is_a_tty && !opts.split_null {
            // Read interactively using reader_readline(). This does not support splitting on null.
            exit_res = read_interactive(
                parser,
                &mut buff,
                opts.nchars,
                opts.shell,
                opts.silent,
                opts.prompt.as_ref().unwrap(),
                &opts.right_prompt,
                &opts.commandline,
                streams.stdin_fd,
            );
        } else if opts.nchars == 0 && !stream_stdin_is_a_tty &&
                   // "one_line" is implemented as reading n-times to a new line,
                   // if we're chunking we could get multiple lines so we would have to advance
                   // more than 1 per run through the loop. Let's skip that for now.
                   !opts.one_line &&
                       (
                           streams.stdin_is_directly_redirected ||
                               unsafe {libc::lseek(streams.stdin_fd, 0, SEEK_CUR)} != -1)
        {
            // We read in chunks when we either can seek (so we put the bytes back),
            // or we have the bytes to ourselves (because it's directly redirected).
            //
            // Note we skip seeking back even if we're directly redirected to a seekable stream,
            // under the assumption that the stream will be closed soon anyway.
            // You don't rewind VHS tapes before throwing them in the trash.
            // TODO: Do this when nchars is set by seeking back.
            exit_res = read_in_chunks(
                streams.stdin_fd,
                &mut buff,
                opts.split_null,
                !streams.stdin_is_directly_redirected,
            );
        } else {
            exit_res =
                read_one_char_at_a_time(streams.stdin_fd, &mut buff, opts.nchars, opts.split_null);
        }

        if exit_res.is_err() {
            clear_remaining_vars(&mut var_ptr);
            return exit_res;
        }

        if opts.to_stdout {
            streams.out.append(buff);
            return exit_res;
        }

        if opts.tokenize {
            let mut tok = Tokenizer::new(&buff, TOK_ACCEPT_UNFINISHED | TOK_ARGUMENT_LIST);
            if opts.array {
                // Array mode: assign each token as a separate element of the sole var.
                let mut tokens = vec![];
                while let Some(t) = tok.next() {
                    let text = tok.text_of(&t);
                    if let Some(out) = unescape_string(text, UnescapeStringStyle::default()) {
                        tokens.push(out);
                    } else {
                        tokens.push(text.to_owned());
                    }
                }

                parser.set_var_and_fire(argv[var_ptr], opts.place, tokens);
                var_ptr += 1;
            } else {
                while vars_left(var_ptr) - 1 > 0 {
                    let Some(t) = tok.next() else {
                        break;
                    };
                    let text = tok.text_of(&t);
                    let out = unescape_string(text, UnescapeStringStyle::default())
                        .unwrap_or_else(|| text.to_owned());
                    parser.set_var_and_fire(argv[var_ptr], opts.place, vec![out]);
                    var_ptr += 1;
                }

                // If we still have tokens, set the last variable to them.
                if let Some(t) = tok.next() {
                    let rest = buff[t.offset()..].to_owned();
                    parser.set_var_and_fire(argv[var_ptr], opts.place, vec![rest]);
                    var_ptr += 1;
                }
            }
            // The rest of the loop is other split-modes, we don't care about those.
            // Make sure to check the loop exit condition before continuing.
            if !opts.one_line || vars_left(var_ptr) == 0 {
                break;
            }
            continue;
        }

        let mut ifs_delimiter = WString::new();
        let delimiter: &wstr = opts.delimiter.as_deref().unwrap_or_else(|| {
            ifs_delimiter = parser
                .vars()
                .get_unless_empty(L!("IFS"))
                .map(|var| var.as_string())
                .unwrap_or_default();
            &ifs_delimiter
        });

        if delimiter.is_empty() {
            // Every character is a separate token with one wrinkle involving non-array mode where
            // the final var gets the remaining characters as a single string.
            let x = 1.max(buff.len());
            let n_splits = if opts.array || vars_left(var_ptr) > x {
                x
            } else {
                vars_left(var_ptr)
            };
            let mut chars = Vec::with_capacity(n_splits);

            for (i, c) in buff.chars().enumerate() {
                if opts.array || i + 1 < vars_left(var_ptr) {
                    chars.push(WString::from_chars([c]));
                } else {
                    chars.push(buff[i..].to_owned());
                    break;
                }
            }

            if opts.array {
                // Array mode: assign each char as a separate element of the sole var.
                parser.set_var_and_fire(argv[var_ptr], opts.place, chars);
                var_ptr += 1;
            } else {
                // Not array mode: assign each char to a separate var with the remainder being
                // assigned to the last var.
                for c in chars {
                    parser.set_var_and_fire(argv[var_ptr], opts.place, vec![c]);
                    var_ptr += 1;
                }
            }
        } else if opts.array {
            // The user has requested the input be split into a sequence of tokens and all the
            // tokens assigned to a single var. How we do the tokenizing depends on whether the user
            // specified the delimiter string or we're using IFS.
            if opts.delimiter.is_none() {
                // We're using IFS, so tokenize the buffer using each IFS char. This is for backward
                // compatibility with old versions of fish.
                let tokens = split_string_tok(&buff, delimiter, None)
                    .into_iter()
                    .map(|s| s.to_owned())
                    .collect();
                parser.set_var_and_fire(argv[var_ptr], opts.place, tokens);
                var_ptr += 1;
            } else {
                // We're using a delimiter provided by the user so use the `string split` behavior.
                let splits = split_about(&buff, delimiter, usize::MAX, false)
                    .into_iter()
                    .map(|s| s.to_owned())
                    .collect();
                parser.set_var_and_fire(argv[var_ptr], opts.place, splits);
                var_ptr += 1;
            }
        } else {
            // Not array mode. Split the input into tokens and assign each to the vars in sequence.
            if opts.delimiter.is_none() {
                // We're using IFS, so tokenize the buffer using each IFS char. This is for backward
                // compatibility with old versions of fish.
                // Note the final variable gets any remaining text.
                let mut var_vals: Vec<WString> =
                    split_string_tok(&buff, delimiter, Some(vars_left(var_ptr)))
                        .into_iter()
                        .map(|s| s.to_owned())
                        .collect();
                let mut val_idx = 0;
                while vars_left(var_ptr) != 0 {
                    let mut val = WString::new();
                    if val_idx < var_vals.len() {
                        std::mem::swap(&mut val, &mut var_vals[val_idx]);
                        val_idx += 1;
                    }
                    parser.set_var_and_fire(argv[var_ptr], opts.place, vec![val]);
                    var_ptr += 1;
                }
            } else {
                // We're using a delimiter provided by the user so use the `string split` behavior.
                // We're making at most argc - 1 splits so the last variable
                // is set to the remaining string.
                let splits = split_about(&buff, delimiter, argc - 1, false);
                assert!(splits.len() <= vars_left(var_ptr));
                for split in splits {
                    parser.set_var_and_fire(argv[var_ptr], opts.place, vec![split.to_owned()]);
                    var_ptr += 1;
                }
            }
        }

        if !opts.one_line || vars_left(var_ptr) == 0 {
            break;
        }
    }

    if !opts.array {
        // In case there were more args than splits
        clear_remaining_vars(&mut var_ptr);
    }

    exit_res
}
