#!/bin/bash
# Opt-in hardware tests. Requires H.264, HEVC and AV1 NVENC/NVDEC support.
# No external media, network access or nvidia-smi required. Missing GPU support
# is a failure, never a skip or a successful software fallback.
set -Eeuo pipefail

WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT
trap 'echo "FAIL: NVIDIA test at line $LINENO" >&2; cat "$WORK_DIR"/*.log >&2' ERR

# Override to exercise larger frames, e.g. NVIDIA_TEST_SIZE=3840x2160.
SIZE=${NVIDIA_TEST_SIZE:-1280x720}
WIDTH=${SIZE%x*}
HEIGHT=${SIZE#*x}
FRAMES=60

check_output() {
  local path=$1 codec=$2 pix_fmt=$3 width=${4:-$WIDTH} height=${5:-$HEIGHT} reference=${6:-$WORK_DIR/input.mp4}
  ffprobe -v error -select_streams v:0 -count_frames \
    -show_entries stream=codec_name,width,height,pix_fmt,nb_read_frames \
    -of json "$path" > "$WORK_DIR/probe.json"
  python3 - "$WORK_DIR/probe.json" "$codec" "$pix_fmt" "$width" "$height" "$FRAMES" <<'PY'
import json
import sys
path, codec, pixel_format, width, height, frames = sys.argv[1:]
streams = json.load(open(path))["streams"]
assert len(streams) == 1, streams
stream = streams[0]
expected = dict(codec_name=codec, pix_fmt=pixel_format, width=int(width),
                height=int(height), nb_read_frames=frames)
for key, value in expected.items():
    assert stream[key] == value, (key, stream[key], value)
PY
  # Compare by frame index: MP4 and Matroska use different timestamp precision,
  # which otherwise makes framesync pair adjacent frames at 30 fps.
  ffmpeg -hide_banner -nostdin -i "$reference" -i "$path" \
    -lavfi "[0:v]format=${pix_fmt},settb=AVTB,setpts=N/(30*TB)[ref];[1:v]format=${pix_fmt},settb=AVTB,setpts=N/(30*TB)[out];[ref][out]psnr" \
    -f null - > "$WORK_DIR/quality.log" 2>&1
  python3 - "$WORK_DIR/quality.log" <<'PY'
import re
import sys
text = open(sys.argv[1]).read()
match = re.search(r"average:([\d.]+|inf)", text)
assert match, text
assert float(match[1]) > 25, match[0]
print("  " + match[0] + " dB")
PY
}

echo "Generating $SIZE / $FRAMES-frame H.264 reference with the software encoder"
ffmpeg -hide_banner -nostdin -v error -f lavfi \
  -i "testsrc2=size=$SIZE:rate=30" -frames:v "$FRAMES" \
  -c:v libx264 -preset fast -crf 18 -profile:v high -pix_fmt yuv420p "$WORK_DIR/input.mp4"

ffmpeg -hide_banner -nostdin -v error -i "$WORK_DIR/input.mp4" \
  -vf scale=640:360:flags=bilinear -c:v ffv1 "$WORK_DIR/scaled-reference.mkv"

for codec in h264 hevc av1; do
  echo "TEST: software decode -> ${codec}_nvenc"
  ffmpeg -hide_banner -nostdin -v verbose -i "$WORK_DIR/input.mp4" \
    -an -c:v "${codec}_nvenc" -preset p4 -rc constqp -qp 20 \
    "$WORK_DIR/upload-$codec.mkv" > "$WORK_DIR/encode.log" 2>&1
  check_output "$WORK_DIR/upload-$codec.mkv" "$codec" yuv420p

  echo "TEST: H.264 NVDEC -> CUDA frames -> ${codec}_nvenc"
  ffmpeg -hide_banner -nostdin -v verbose \
    -hwaccel cuda -hwaccel_output_format cuda -c:v h264_cuvid \
    -i "$WORK_DIR/input.mp4" -an \
    -c:v "${codec}_nvenc" -preset p4 -rc constqp -qp 20 \
    "$WORK_DIR/transcode-$codec.mkv" > "$WORK_DIR/transcode.log" 2>&1
  check_output "$WORK_DIR/transcode-$codec.mkv" "$codec" yuv420p

  echo "TEST: NVDEC -> scale_cuda 640x360 -> ${codec}_nvenc"
  ffmpeg -hide_banner -nostdin -v verbose \
    -hwaccel cuda -hwaccel_output_format cuda -i "$WORK_DIR/input.mp4" \
    -vf scale_cuda=640:360:interp_algo=bilinear -an \
    -c:v "${codec}_nvenc" -preset p4 -rc constqp -qp 20 \
    "$WORK_DIR/scaled-$codec.mkv" > "$WORK_DIR/scale.log" 2>&1
  check_output "$WORK_DIR/scaled-$codec.mkv" "$codec" yuv420p 640 360 "$WORK_DIR/scaled-reference.mkv"

  echo "TEST: $codec NVDEC -> hwdownload (all $FRAMES frames)"
  # hwdownload requires hardware frames: a software fallback cannot pass.
  ffmpeg -hide_banner -nostdin -v verbose \
    -hwaccel cuda -hwaccel_output_format cuda -c:v "${codec}_cuvid" \
    -i "$WORK_DIR/transcode-$codec.mkv" -an -vf hwdownload,format=nv12 \
    -fps_mode passthrough -f rawvideo "$WORK_DIR/decoded.yuv" > "$WORK_DIR/decode.log" 2>&1
  test "$(stat -c %s "$WORK_DIR/decoded.yuv")" -eq "$((WIDTH * HEIGHT * 3 * FRAMES / 2))"
  # Also compare GPU-decoded pixels with a software decode of the same bitstream.
  ffmpeg -hide_banner -nostdin -f rawvideo -pixel_format nv12 -video_size "$SIZE" \
    -framerate 30 -i "$WORK_DIR/decoded.yuv" -i "$WORK_DIR/transcode-$codec.mkv" \
    -lavfi '[0:v]format=yuv420p,settb=AVTB,setpts=N/(30*TB)[gpu];[1:v]format=yuv420p,settb=AVTB,setpts=N/(30*TB)[cpu];[gpu][cpu]psnr' \
    -f null - > "$WORK_DIR/decode-quality.log" 2>&1
  python3 - "$WORK_DIR/decode-quality.log" <<'PY'
import re
import sys
text = open(sys.argv[1]).read()
match = re.search(r"average:([\d.]+|inf)", text)
assert match, text
assert float(match[1]) > 45, match[0]
print("  GPU/software decode " + match[0] + " dB")
PY
  rm "$WORK_DIR/decoded.yuv"
  echo "PASS: $codec encode, GPU transcode, and GPU decode"
done

echo "All NVIDIA hardware tests passed (12 paths)."
