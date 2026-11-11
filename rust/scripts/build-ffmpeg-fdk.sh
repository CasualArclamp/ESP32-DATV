#!/bin/bash
# Builds an FFmpeg with libfdk_aac (for HE-AAC) and the encoders DATV TX uses, in MSYS2's
# MINGW64 environment. Nothing is installed into MSYS2: the sources and a private prefix
# live in the work directory, and the result is copied with the DLLs it needs.
#
# libfdk_aac makes the binary "non-free" (configure --enable-nonfree): it is for your own
# use and must not be redistributed. Do not commit or upload the result.
#
# MSYS2 packages needed (mingw-w64-x86_64-...): gcc make nasm pkgconf cmake ninja fdk-aac
# libx264 x265 svt-av1 opus lame dav1d, and git from anywhere on PATH.
#
#   C:\msys64\usr\bin\env MSYSTEM=MINGW64 /usr/bin/bash -l rust/scripts/build-ffmpeg-fdk.sh WORKDIR OUTDIR
set -euo pipefail
WORK=${1:?work directory}
OUT=${2:?output directory}
FFMPEG_BRANCH=${FFMPEG_BRANCH:-release/8.1}
JOBS=${JOBS:-$(nproc)}
mkdir -p "$WORK" "$OUT"
cd "$WORK"
PREFIX=$PWD/prefix
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig:${PKG_CONFIG_PATH:-}"

clone() { # url directory [branch]
    [ -d "$2/.git" ] || git clone --depth 1 ${3:+--branch "$3"} "$1" "$2"
}

echo "== NVIDIA encoder headers"
clone https://github.com/FFmpeg/nv-codec-headers.git nv-codec-headers
make -C nv-codec-headers install PREFIX="$PREFIX" >/dev/null

echo "== vvenc (H.266 encoder; MSYS2 has no package)"
clone https://github.com/fraunhoferhhi/vvenc.git vvenc
if [ ! -f "$PREFIX/lib/pkgconfig/libvvenc.pc" ]; then
    cmake -S vvenc -B vvenc/build -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$PREFIX" \
        -DBUILD_SHARED_LIBS=OFF -DVVENC_ENABLE_LINK_TIME_OPT=OFF -DVVENC_LIBRARY_ONLY=ON >/dev/null
    cmake --build vvenc/build -j "$JOBS" >/dev/null
    cmake --install vvenc/build >/dev/null
fi

echo "== FFmpeg $FFMPEG_BRANCH"
clone https://github.com/FFmpeg/FFmpeg.git ffmpeg "$FFMPEG_BRANCH"
cd ffmpeg
./configure --prefix="$PREFIX" --arch=x86_64 --target-os=mingw32 \
    --enable-gpl --enable-version3 --enable-nonfree \
    --enable-libfdk-aac --enable-libx264 --enable-libx265 --enable-libsvtav1 --enable-libvvenc \
    --enable-libopus --enable-libmp3lame --enable-libdav1d --enable-nvenc \
    --disable-doc --disable-debug --disable-ffplay --disable-autodetect \
    --enable-schannel --enable-d3d11va --enable-dxva2 \
    --extra-libs="-lstdc++ -lpthread" --pkg-config-flags=--static
make -j "$JOBS"
cd ..

echo "== collect"
cp ffmpeg/ffmpeg.exe ffmpeg/ffprobe.exe "$OUT/"
# the MinGW DLLs the programs load (Windows' own are left where they are)
for exe in "$OUT/ffmpeg.exe" "$OUT/ffprobe.exe"; do
    ldd "$exe" | awk '$3 ~ /^\/mingw64\// { print $3 }' | sort -u | while read -r dll; do
        cp -u "$dll" "$OUT/"
    done
done
"$OUT/ffmpeg.exe" -hide_banner -encoders | grep -E "libfdk_aac|libx264 |libx265|libsvtav1|libvvenc|libopus|libmp3lame|_nvenc"
echo "done: $OUT/ffmpeg.exe"
