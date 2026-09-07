#!/bin/sh
# Downloads everything IRA needs into ./models and ./piper
# Run from the repo root:  ./scripts/fetch-models.sh
#
# The POSIX counterpart of fetch-models.ps1. Keep the two in step: they fetch
# the same files, from the same pinned releases, into the same directories.
#
# Add --whisper for offline STT (see "Local STT" in the README). It is opt-in
# because it is a much larger download than everything else here combined, and
# IRA talks to Groq by default -- nothing breaks without it.
#
#   ./scripts/fetch-models.sh --whisper
#   ./scripts/fetch-models.sh --whisper --model small.en

set -eu

WHISPER=0
MODEL=""
while [ $# -gt 0 ]; do
    case "$1" in
        --whisper) WHISPER=1 ;;
        --model) MODEL="${2:-}"; shift ;;
        -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
models="$root/models"
mkdir -p "$models"

fetch() {
    # $1 url, $2 destination
    if [ -f "$2" ]; then
        echo "have  $(basename "$2")"
        return
    fi
    echo "fetch $(basename "$2")"
    # -f so an HTML error page is not written out as if it were a model.
    curl -fsSL "$1" -o "$2"
}

# --- openWakeWord (Apache-2.0): shared feature extractors + the wake model ---
oww="https://github.com/dscripka/openWakeWord/releases/download/v0.5.1"
fetch "$oww/melspectrogram.onnx"  "$models/melspectrogram.onnx"
fetch "$oww/embedding_model.onnx" "$models/embedding_model.onnx"
fetch "$oww/hey_jarvis_v0.1.onnx" "$models/hey_jarvis_v0.1.onnx"

# --- Silero VAD v5 (MIT) ---
fetch "https://raw.githubusercontent.com/snakers4/silero-vad/master/src/silero_vad/data/silero_vad.onnx" \
      "$models/silero_vad.onnx"

# --- Piper voice (MIT model, CC-BY dataset) ---
voices="https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/amy/medium"
fetch "$voices/en_US-amy-medium.onnx"      "$models/en_US-amy-medium.onnx"
fetch "$voices/en_US-amy-medium.onnx.json" "$models/en_US-amy-medium.onnx.json"

# --- Piper binary ---
if [ ! -x "$root/piper/piper" ]; then
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64)  asset=piper_linux_x86_64.tar.gz ;;
        Linux-aarch64) asset=piper_linux_aarch64.tar.gz ;;
        Darwin-x86_64) asset=piper_macos_x64.tar.gz ;;
        Darwin-arm64)  asset=piper_macos_aarch64.tar.gz ;;
        *) echo "no prebuilt piper for $(uname -s)-$(uname -m); set IRA_PIPER" >&2; exit 1 ;;
    esac
    echo "fetch $asset"
    tmp=$(mktemp -d)
    # Never cached: a half-downloaded archive extracts a half-populated piper/
    # and still looks like it worked.
    curl -fsSL "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/$asset" \
         -o "$tmp/piper.tar.gz"
    tar -xzf "$tmp/piper.tar.gz" -C "$root"
    rm -rf "$tmp"
    # Check the one file that matters rather than trusting the extraction.
    [ -x "$root/piper/piper" ] || {
        echo "piper missing after extraction -- delete the piper folder and re-run" >&2
        exit 1
    }
fi

# --- whisper.cpp for offline STT (opt-in) ---
if [ "$WHISPER" = "1" ]; then
    # No prebuilt whisper.cpp binaries are published for Linux or macOS, so this
    # builds from source. cmake and a C++ compiler are required; on macOS the
    # build picks up Metal, which makes the GPU question answer itself.
    if [ -z "$MODEL" ]; then MODEL=base.en; fi
    if [ ! -x "$root/whisper/whisper-server" ]; then
        echo "build whisper.cpp (needs cmake and a C++ compiler)"
        tmp=$(mktemp -d)
        git clone --depth 1 --branch v1.7.6 https://github.com/ggml-org/whisper.cpp "$tmp/w"
        # Static: the binaries are copied out of the build tree below, and a
        # shared build leaves them looking for libwhisper.so on a path that no
        # longer exists. ELF has no "next to the executable" search rule, so
        # this failed on Linux while working on Windows, where DLLs do work
        # that way.
        cmake -S "$tmp/w" -B "$tmp/w/build" -DCMAKE_BUILD_TYPE=Release \
              -DBUILD_SHARED_LIBS=OFF >/dev/null
        cmake --build "$tmp/w/build" --config Release -j >/dev/null
        mkdir -p "$root/whisper"
        find "$tmp/w/build" -type f -perm -u+x -name 'whisper-*' \
            -exec cp {} "$root/whisper/" \;
        rm -rf "$tmp"
        # Run it, rather than looking at it. A binary that exists and cannot
        # start is what this catches, and it is otherwise invisible until
        # something tries to transcribe and gets a connection refused.
        "$root/whisper/whisper-server" --help >/dev/null 2>&1 || {
            echo "whisper-server was built but will not run" >&2
            exit 1
        }
    else
        echo "have  whisper-server"
    fi
    fetch "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-$MODEL.bin" \
          "$models/ggml-$MODEL.bin"
fi

echo
echo "done. now:"
echo '  export ANTHROPIC_API_KEY="sk-ant-..."'
if [ "$WHISPER" = "1" ]; then
    echo
    echo '  # offline STT -- leave this running in its own terminal:'
    echo "  ./whisper/whisper-server -m ./models/ggml-$MODEL.bin --host 127.0.0.1 --port 8231"
    echo '  export IRA_STT_URL="http://127.0.0.1:8231/inference"'
    echo
else
    echo '  export GROQ_API_KEY="gsk_..."'
fi
echo "  cargo run --release"
