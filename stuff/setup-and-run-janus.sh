#!/usr/bin/env bash

# Helper script for the lazy and not picky. Compiles, installs, and runs Janus, its dependencies,
# and the latest version of this plugin. Should be compatible with Ubuntu >= 16.04.
#
# Janus will be installed into /opt/janus.

set -e

banner () {
    msg=$1
    echo ''
    echo '####################################################'
    echo "    $msg"
    echo '####################################################'
    echo ''
}

banner 'installing script dependencies'
sudo apt update
sudo apt -y install python || true

if [[ ! -e $(which docopts) ]]; then
    curl https://bootstrap.pypa.io/get-pip.py -sSf > get-pip.py
    sudo python get-pip.py
    rm get-pip.py
    sudo pip install docopts
fi

eval "$(
docopts -h - : "$@" <<EOF
Usage: ./setup-and-run-social-mr-janus-server.sh [--force-rebuild] [--working-directory <dir>]

    -f --force-rebuild               forcefully rebuild dependencies
    -d --working-directory <dir>     directory to work under [default: ./]
EOF
)"

working_directory=$(realpath "$working_directory")
force_rebuild=$([[ $force_rebuild == "true" ]] && echo "true") || true

cd "$working_directory"

git-get () {
    repo=$1
    version=$2
    if [ ! -e $repo ]; then
        git clone https://github.com/$repo $repo
    fi
    cd $repo
    git checkout master
    git pull
    git checkout -f $version
    git clean -fdx
}

if [[ $force_rebuild || ! -e /opt/janus/bin/janus ]]; then
    banner 'getting janus source'
    git-get meetecho/janus-gateway v0.2.4
    cd "$working_directory"

    banner 'installing janus compilation dependencies'
    sudo apt -y install dh-autoreconf pkg-config libglib2.0-dev \
        libjansson-dev libnice-dev libssl-dev gengetopt libmicrohttpd-dev cmake

    if [[ $force_rebuild || ! -e /usr/lib/libsrtp.so ]]; then
        git-get cisco/libsrtp v2.0.0
        ./configure --prefix=/usr --enable-openssl
        make shared_library && sudo make install
        cd "$working_directory"
    fi

    if [[ $force_rebuild || ! -e /usr/lib/libusrsctp.so ]]; then
        git-get sctplab/usrsctp 2d26613
        ./bootstrap
        ./configure --prefix=/usr && make && sudo make install
        cd "$working_directory"
    fi

    if [[ $force_rebuild || ! -e /usr/lib/libwebsockets.so ]]; then
        git-get warmcat/libwebsockets v2.0.0
        mkdir build
        cd build
        cmake -DCMAKE_INSTALL_PREFIX:PATH=/usr -DCMAKE_C_FLAGS="-fpic" ..
        make && sudo make install
        cd "$working_directory"
    fi

    banner 'building and installing janus'
    cd meetecho/janus-gateway
    sh autogen.sh
    ./configure --prefix=/opt/janus
    make
    sudo make install
    sudo make configs
    cd "$working_directory"
fi

if [[ $force_rebuild || ! -e /opt/janus/lib/janus/plugins/libjanus_plugin_sfu.so ]]; then
    banner 'installing latest rust'
    curl https://sh.rustup.rs -sSf > rustup.sh
    sh rustup.sh -y
    . ~/.cargo/env
    rm rustup.sh

    banner 'getting, building and installing janus-plugin-sfu'
    git-get mquander/janus-plugin-sfu master
    cargo build --release
    sudo cp target/release/libjanus_plugin_sfu.so /opt/janus/lib/janus/plugins/
    cd "$working_directory"
fi

if [ "$(awk '/\[plugins\]/,/^disable/' /opt/janus/etc/janus/janus.cfg | wc -l)" -gt "2" ]; then
    sudo sed 's/\[plugins\]/\0\ndisable = '\
'libjanus_voicemail.so,libjanus_echotest.so,libjanus_recordplay.so,libjanus_streaming.so,'\
'libjanus_textroom.so,libjanus_videocall.so,libjanus_videoroom.so/' -i /opt/janus/etc/janus/janus.cfg
fi

banner 'starting janus and web servers'
/opt/janus/bin/janus &
cd mquander/janus-plugin-sfu/demo
python -m SimpleHTTPServer &
cd "$working_directory"

trap "kill %1; kill %2; wait" SIGINT
sleep 1
banner 'press Ctrl+C to kill'
wait
