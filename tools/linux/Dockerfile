# Linux build and test environment for pingpong (see tools/linux-dev).
FROM ubuntu:24.04
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates curl build-essential pkg-config clang libclang-dev \
        libavcodec-dev libavutil-dev libswscale-dev libva-dev \
        libasound2-dev libpulse-dev libudev-dev \
        libx11-dev libx11-xcb-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
        libxcursor-dev libxrandr-dev libxi-dev libfontconfig-dev libfreetype-dev libxcb1-dev \
        mesa-vulkan-drivers libvulkan1 libgl1-mesa-dri libegl1 xvfb xauth pulseaudio pulseaudio-utils \
        libpipewire-0.3-dev libspa-0.2-dev libdbus-1-dev \
        dbus dbus-user-session pipewire wireplumber xwayland \
        xdg-desktop-portal mutter xdg-desktop-portal-gnome libglib2.0-bin \
        openbox tint2 xterm mousepad galculator pcmanfm fonts-dejavu-core \
        xdotool x11-utils scrot socat python3 \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --component clippy --default-toolchain stable
ENV PATH=/root/.cargo/bin:$PATH
