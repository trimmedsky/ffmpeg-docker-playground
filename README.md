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

## Testing

Three suites, each answering a different question. All of them run in CI on every branch
(`.github/workflows/test-build.yml`), and all of them can be run locally against an image
you just built.

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

## Update libraries

Modify version number `ARG`s in `Dockerfile`, then run `docker build` and the test suites again.
