# `ffmpeg` custom build dockerfile recipe

Encapsulate `ffmpeg` stuff into Docker image.

This repository provides [Dockerfile](./Dockerfile) so that you can build docker image contains `ffmpeg` binary easily.

Caution: should not redistribute resulted container image unless you are sure licence terms of ALL libraries used in this build.

## Usage

```
docker build . -t ffmpeg    # take a coffee break

# To run command in generated container, just use `docker run`
# Some examples are placed at sample-outputs/*/*.sh
```

The same Dockerfile builds natively on Linux `amd64` and `arm64`, with the software
codecs and NVIDIA NVENC/NVDEC enabled on both architectures. No GPU or CUDA toolkit
is needed to build the image or use the software codecs.

### NVIDIA hardware acceleration

If you use NVIDIA hardware acceleration (NVDEC/NVENC), the host needs a supported
NVIDIA GPU, a Linux NVIDIA driver **570 or newer**
([nv-codec-headers 13.0.19 requirements](https://github.com/FFmpeg/nv-codec-headers/tree/n13.0.19.0)),
and [NVIDIA Container Toolkit configured for Docker](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html).
Individual GPUs support different codecs; AV1 encoding requires newer hardware.
NVENC does not provide a VP9 encoder; `libvpx-vp9` remains available on the CPU.

Request both `compute` (CUDA) and `video` (codec driver libraries). The default
Container Toolkit capabilities omit `video`; see the
[driver capability documentation](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/docker-specialized.html#driver-capabilities).

```sh
docker run --rm --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=compute,video,utility \
  --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work \
  ffmpeg ffmpeg -hwaccel cuda -hwaccel_output_format cuda -i input.mp4 \
    -c:v h264_nvenc -preset p4 -cq 23 -c:a copy output.mp4
```

Use `hevc_nvenc` or `av1_nvenc` for those output codecs on supported GPUs. If the
input cannot be decoded by NVDEC, omit the two `-hwaccel*` options to decode on the
CPU and still encode with NVENC. GPU frames stay on the GPU in the command above;
CPU filters need an explicit `hwdownload` / format conversion / upload path.
CUDA filters including `scale_cuda` are built with Clang (`--enable-cuda-llvm`),
without the CUDA Toolkit. For example, add `-vf scale_cuda=1280:720` before
`-c:v` above to resize GPU frames. `scale_cuda` supports resizing and compatible
pixel format conversions;
it does not convert between YUV and RGB (see the
[FFmpeg filter documentation](https://ffmpeg.org/ffmpeg-filters.html#scale_005fcuda)).
See [NVIDIA's FFmpeg guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/ffmpeg-with-nvidia-gpu/index.html)
for the decode and encode model.

## Optional HTTP agent

The image also installs `ffmpeg-agent`, a small Rust worker for bounded, stateless
GET → FFmpeg → PUT jobs with progress and completion callbacks. It starts only
when explicitly selected as the container command. See [agent/README.md](agent/README.md)
for the API, configuration, license, tests and a systemd/Docker unit.

## Testing

Three CPU suites run in CI on non-master branch pushes on native AMD64 and ARM64
runners (`.github/workflows/test-build.yml`). They require no GPU, including the
checks for compiled NVIDIA support. A fourth, opt-in suite tests real GPU operation.
All suites can be run locally against an image you just built.

### 1. Is the binary built the way it should be? (build tests)

```
docker run --rm \
  -v "$PWD/tests:/tests:ro" \
  ffmpeg bash /tests/run-build-tests.sh
```

### 2. Can it encode/decode the formats at all? (capability tests)

```
docker run --rm \
  -v "$PWD/test-media:/test-media:ro" -v "$PWD/tests:/tests:ro" \
  ffmpeg bash /tests/run-media-tests.sh
```

### 3. Does it still produce the SAME files? (profile regression tests)

```
docker run --rm \
  -v "$PWD/test-media:/test-media:ro" -v "$PWD/tests:/tests:ro" \
  ffmpeg bash /tests/run-profile-regression-tests.sh
```

Runs complete transcode profiles - the argument sets a downstream media library uses in
production - end to end, and examines the resulting files rather than the exit code:
container and codec facts exactly (profile, level, codec tag, pixel format, channel
layout, `moov` before `mdat`), plus PSNR/SSIM and spectrogram-SSIM/SDR against the input
within generous bands.

This catches what the capability tests cannot: an argument that became a no-op, a
container default that moved, an encoder that started reporting a different profile or
tag, or decode-side behaviour (display-matrix rotation, attached-picture extraction) that
changed. Nothing is compared against recorded numbers, because upgrading ffmpeg is the
point of this repository - the perceptual thresholds are floors far below a working
encoder and far above a broken argument set. Test material is what is already in
`test-media/` plus clips the script generates with ffmpeg itself into a temporary
directory.

### NVIDIA runtime tests (GPU required)

```sh
docker run --rm --gpus all --network none \
  -e NVIDIA_DRIVER_CAPABILITIES=compute,video,utility \
  --user "$(id -u):$(id -g)" -v "$PWD/tests:/tests:ro" \
  ffmpeg bash /tests/run-nvidia-tests.sh
```

Requires a GPU supporting H.264, HEVC and AV1 encode/decode. Synthetic input is
generated inside the disposable container. For each codec the suite tests CPU
decode → NVENC, NVDEC → GPU frames → NVENC, and NVDEC → `hwdownload`. It checks the
output codec, dimensions, pixel format, all 60 decoded frames and PSNR above 25 dB.
GPU-decoded pixels must also agree with software decoding above 45 dB PSNR.
Missing GPU support fails the suite; it does not skip tests or accept software
fallback. Set `-e NVIDIA_TEST_SIZE=3840x2160` for a 4K run (default: 1280×720).
Listing encoders alone is not evidence that the host can use them.

## Update libraries

Modify version number `ARG`s in `Dockerfile`, then run `docker build` and the test suites again.
