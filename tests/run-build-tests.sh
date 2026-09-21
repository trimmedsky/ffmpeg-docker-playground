#!/bin/bash
set -euo pipefail

# Build tests for the ffmpeg Docker image
#
# "Is the binary built the way it should be?" - checks the ffmpeg/ffprobe binaries
# themselves (location, build options, codec table, linked libraries) rather than
# what they can actually encode/decode (see run-media-tests.sh for that).
#
# Run inside the container, no test-media required:
#   docker run --rm -v "$PWD/tests:/tests:ro" ffmpeg-test bash /tests/run-build-tests.sh

PASSED=0
FAILED=0
TOTAL=0

# --- Helper Functions ---

run_test() {
  local name="$1"
  shift
  TOTAL=$((TOTAL + 1))
  echo -n "  TEST: $name ... "
  if "$@"; then
    PASSED=$((PASSED + 1))
    echo "PASS"
  else
    FAILED=$((FAILED + 1))
    echo "FAIL"
  fi
}

assert_command_path() {
  local cmd="$1" expected="$2"
  local actual
  actual=$(command -v "$cmd") || return 1
  [ "$actual" = "$expected" ]
}

assert_buildconf_has() {
  local option="$1"
  # Grepping via a pipe here would race: -q exits as soon as it matches, and with
  # `pipefail` a SIGPIPE'd producer turns a real match into a reported failure.
  # A herestring has no pipe (bash spools it to a temp file/fd), so no such race.
  grep -qF -- "$option" <<<"$BUILDCONF_OUTPUT"
}

assert_pkg_config() {
  local lib="$1"
  local out err
  out=$(pkg-config "$lib" --modversion 2>/tmp/pkg-config-err) || return 1
  err=$(cat /tmp/pkg-config-err)
  rm -f /tmp/pkg-config-err
  [ -z "$err" ] && [ -n "$out" ]
}

assert_ffmpeg_version_lists_libav() {
  local version_output
  version_output=$(ffmpeg -version 2>&1)
  grep -q "^lib" <<<"$version_output"
}

# --- Executable location tests ---

echo "=== Executable Location Tests ==="

run_test "ffmpeg is /usr/local/bin/ffmpeg" assert_command_path ffmpeg /usr/local/bin/ffmpeg
run_test "ffprobe is /usr/local/bin/ffprobe" assert_command_path ffprobe /usr/local/bin/ffprobe

# --- Build option tests ---

echo ""
echo "=== Build Option Tests ==="

BUILDCONF_OUTPUT=$(ffmpeg -buildconf 2>&1)

BUILD_OPTIONS=(
  --enable-shared
  --disable-debug
  --disable-doc
  --disable-ffplay
  --enable-gpl
  --enable-nonfree
  --enable-version3
  --enable-pthreads
  --enable-ffnvcodec
  --enable-cuda
  --enable-nvenc
  --enable-nvdec
  --enable-cuvid
)

for option in "${BUILD_OPTIONS[@]}"; do
  run_test "buildconf contains '$option'" assert_buildconf_has "$option"
done

# --- Codec table tests ---

echo ""
echo "=== Codec Table Tests ==="

CODECS_OUTPUT=$(ffmpeg -codecs 2>/dev/null)

# ffmpeg -codecs flag columns, left to right:
#   D..... = Decoding supported
#   .E.... = Encoding supported
#   ..V... = Video codec (A = Audio, S = Subtitle)
#   ...I.. = Intra frame-only codec
#   ....L. = Lossy compression
#   .....S = Lossless compression

CODEC_FLAGS=(
  "flv1 DEV.L."
  "h264 DEV.LS"
  "hevc DEV.L." # H.265 / HEVC
  "mpeg1video DEV.L."
  "mpeg2video DEV.L."
  "mpeg4 DEV.L."
  "theora DEV.L."
  "vp8 DEV.L."
  "vp9 DEV.L."
  "wmv1 DEV.L."
  "wmv2 DEV.L."
  "wmv3 D.V.L."
  "aac DEA.L." # (decoders: aac aac_fixed libfdk_aac ) (encoders: aac libfdk_aac )
  "alac DEAI.S" # Apple Lossless
  "amr_nb D.AIL."
  "amr_wb D.AIL."
  "ape D.AI.S"
  "flac DEAI.S"
  "mp3 DEAIL." # (decoders: mp3 mp3float ) (encoders: libmp3lame )
  "opus DEAIL." # (decoders: opus libopus ) (encoders: opus libopus )
  "vorbis DEAIL." # (decoders: vorbis libvorbis ) (encoders: vorbis libvorbis )
  "wmapro D.AIL."
  "wmav1 DEAIL."
  "wmav2 DEAIL."
)

assert_codec_flags() {
  local name="$1" flags="$2"
  # Herestring, not a pipe: see the comment on assert_buildconf_has.
  grep -qE "^ ${flags} ${name} " <<<"$CODECS_OUTPUT"
}

for entry in "${CODEC_FLAGS[@]}"; do
  name="${entry%% *}"
  flags="${entry#* }"
  run_test "codec $name ($flags)" assert_codec_flags "$name" "$flags"
done

assert_encoder() {
  grep -qE "\(encoders:[^)]*$1[^)]*\)" <<<"$CODECS_OUTPUT"
}

assert_decoder() {
  grep -qE "\(decoders:[^)]*$1[^)]*\)" <<<"$CODECS_OUTPUT"
}

for encoder in libx264 libx264rgb libx265 libfdk_aac libmp3lame libopus libvorbis h264_nvenc hevc_nvenc av1_nvenc; do
  run_test "encoder \"$encoder\" listed" assert_encoder "$encoder"
done

for decoder in h264 hevc aac aac_fixed mp3 opus vorbis h264_cuvid hevc_cuvid av1_cuvid; do
  run_test "decoder \"$decoder\" listed" assert_decoder "$decoder"
done

# Listing compiled support must work without a GPU or host driver libraries.
HWACCELS_OUTPUT=$(ffmpeg -hwaccels 2>/dev/null)
run_test "CUDA hardware acceleration listed" grep -qx cuda <<<"$HWACCELS_OUTPUT"

# --- pkg-config tests ---

echo ""
echo "=== pkg-config Tests ==="

# Enabled by build option: filter, swresample, swscale (libpostproc was removed upstream in FFmpeg 8.0)
for lib in libavcodec libavfilter libavformat libavutil libswresample libswscale; do
  run_test "pkg-config $lib" assert_pkg_config "$lib"
done

# --- Linked libraries tests ---

echo ""
echo "=== Linked Libraries Tests ==="

for lib in freetype2 harfbuzz fribidi fontconfig libass x264 x265 ogg vorbis theora \
  fdk-aac opus vpx SvtAv1Enc dav1d libwebp libheif ffnvcodec; do
  run_test "pkg-config $lib" assert_pkg_config "$lib"
done

run_test "ffmpeg -version lists libav*" assert_ffmpeg_version_lists_libav
run_test "heif-enc --version runs" bash -c 'heif-enc --version >/dev/null 2>&1'
run_test "heif-dec --version runs" bash -c 'heif-dec --version >/dev/null 2>&1'

# --- Summary ---

echo ""
echo "==============================="
echo "  Results: $PASSED/$TOTAL passed, $FAILED failed"
echo "==============================="

if [ "$FAILED" -gt 0 ]; then
  exit 1
fi
