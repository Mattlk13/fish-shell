FROM ubuntu:18.04

ENV LANG C.UTF-8
ENV LC_ALL C.UTF-8
ENV CXXFLAGS "-fsanitize=thread"

RUN apt-get update \
  && apt-get -y install \
    build-essential \
    cmake \
    gettext \
    git \
    libncurses5-dev \
    locales \
    ninja-build \
    python3 \
    python3-pexpect \
  && locale-gen en_US.UTF-8

RUN groupadd -g 1000 fishuser \
  && useradd -d /home/fishuser -m -u 1000 -g 1000 fishuser \
  && adduser fishuser sudo \
  && mkdir -p /home/fishuser/fish-build \
  && mkdir /fish-source \
  && chown -R fishuser:fishuser /home/fishuser /fish-source

USER fishuser
WORKDIR /home/fishuser

COPY fish_run_tests.sh /

CMD /fish_run_tests.sh
