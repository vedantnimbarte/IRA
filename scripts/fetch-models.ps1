# Downloads everything IRA needs into .\models and .\piper
# Run from the repo root:  .\scripts\fetch-models.ps1

#
# Add -Whisper for offline STT (see "Local STT" in the README). It is opt-in
# because it is a much larger download than everything else here combined, and
# IRA talks to Groq by default -- nothing breaks without it.
#
#   .\scripts\fetch-models.ps1 -Whisper                  # detect GPU, pick a pack
#   .\scripts\fetch-models.ps1 -Whisper -Backend cpu     # force the CPU build
#   .\scripts\fetch-models.ps1 -Whisper -Model small.en  # pick the model yourself

[CmdletBinding()]
param(
    [switch]$Whisper,
    [ValidateSet('auto', 'cpu', 'cuda11', 'cuda12')]
    [string]$Backend = 'auto',
    [ValidateSet('tiny.en', 'base.en', 'small.en', 'medium.en', 'tiny', 'base', 'small', 'medium')]
    [string]$Model
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$models = Join-Path $root 'models'
New-Item -ItemType Directory -Force -Path $models | Out-Null

function Get-File($url, $dest) {
    if (Test-Path $dest) { Write-Host "have  $(Split-Path -Leaf $dest)"; return }
    Write-Host "fetch $(Split-Path -Leaf $dest)"
    Invoke-WebRequest -Uri $url -OutFile $dest -UseBasicParsing
}

# --- openWakeWord (Apache-2.0): shared feature extractors + the wake model ---
$oww = 'https://github.com/dscripka/openWakeWord/releases/download/v0.5.1'
Get-File "$oww/melspectrogram.onnx"  (Join-Path $models 'melspectrogram.onnx')
Get-File "$oww/embedding_model.onnx" (Join-Path $models 'embedding_model.onnx')
Get-File "$oww/hey_jarvis_v0.1.onnx" (Join-Path $models 'hey_jarvis_v0.1.onnx')

# --- Silero VAD v5 (MIT) ---
Get-File 'https://raw.githubusercontent.com/snakers4/silero-vad/master/src/silero_vad/data/silero_vad.onnx' `
         (Join-Path $models 'silero_vad.onnx')

# --- Piper voice (MIT model, CC-BY dataset) ---
$voices = 'https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/amy/medium'
Get-File "$voices/en_US-amy-medium.onnx"      (Join-Path $models 'en_US-amy-medium.onnx')
Get-File "$voices/en_US-amy-medium.onnx.json" (Join-Path $models 'en_US-amy-medium.onnx.json')

# --- Piper binary ---
$piperExe = Join-Path $root 'piper\piper.exe'
if (-not (Test-Path $piperExe)) {
    # Downloaded fresh every time, never cached: TEMP is shared, and reusing a
    # half-downloaded zip extracts a half-populated piper/ and still "succeeds".
    $zip = Join-Path $env:TEMP "piper_$([guid]::NewGuid()).zip"
    Write-Host 'fetch piper_windows_amd64.zip (~22 MB)'
    Invoke-WebRequest -Uri 'https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_windows_amd64.zip' `
                      -OutFile $zip -UseBasicParsing
    Expand-Archive -Path $zip -DestinationPath $root -Force
    Remove-Item $zip

    # Expand-Archive can partially extract without throwing, so check the one
    # file that actually matters instead of trusting it.
    if (-not (Test-Path $piperExe)) {
        throw "piper.exe missing after extraction. If piper\espeak-ng-data exists but is empty, the archive extracted partially -- delete the piper folder and re-run."
    }
}

# --- whisper.cpp for offline STT (opt-in) ---
$whisperTag = 'v1.7.6'
$whisperDir = Join-Path $root 'whisper'

# The CUDA generation the *driver* supports, or $null for no usable NVIDIA GPU.
# nvidia-smi ships with every NVIDIA driver, so its absence is a reliable "no
# card here" rather than something to warn about.
function Get-CudaMajor {
    if (-not (Get-Command nvidia-smi -ErrorAction SilentlyContinue)) { return $null }
    $out = & nvidia-smi 2>$null | Out-String
    if ($LASTEXITCODE -ne 0) { return $null }
    # Header line reads: "... CUDA Version: 12.4 ..."
    if ($out -match 'CUDA Version:\s*(\d+)') { return [int]$Matches[1] }
    return $null
}

function Resolve-Backend {
    if ($Backend -ne 'auto') { return $Backend }
    $major = Get-CudaMajor
    if ($null -eq $major) {
        Write-Host 'gpu   none detected -- CPU build'
        return 'cpu'
    }
    # cuda11 on a 12.x driver on purpose: CUDA is backward compatible, and the
    # 11.8 pack is 45 MB against cuda12's 443 MB for the same speed on any card
    # CUDA 11.8 has kernels for.
    #
    # ponytail: that excludes cards newer than CUDA 11.8 (Blackwell, sm_120),
    # which fail at load rather than falling back. Pass -Backend cuda12 there.
    Write-Host "gpu   NVIDIA, driver reports CUDA $major.x -- cuBLAS 11.8 build"
    return 'cuda11'
}

if ($Whisper) {
    $resolved = Resolve-Backend
    $asset = switch ($resolved) {
        'cpu'    { 'whisper-bin-x64.zip' }
        'cuda11' { 'whisper-cublas-11.8.0-bin-x64.zip' }
        'cuda12' { 'whisper-cublas-12.4.0-bin-x64.zip' }
    }
    # GPU makes a bigger model affordable, and small.en keeps proper nouns that
    # tiny.en drops. On CPU the same model is too slow to talk to.
    if (-not $Model) { $Model = if ($resolved -eq 'cpu') { 'tiny.en' } else { 'small.en' } }

    $server = Join-Path $whisperDir 'whisper-server.exe'
    if (-not (Test-Path $server)) {
        New-Item -ItemType Directory -Force -Path $whisperDir | Out-Null
        $zip = Join-Path $env:TEMP "whisper_$([guid]::NewGuid()).zip"
        $stage = Join-Path $env:TEMP "whisper_$([guid]::NewGuid())"
        Write-Host "fetch $asset"
        Invoke-WebRequest -Uri "https://github.com/ggml-org/whisper.cpp/releases/download/$whisperTag/$asset" `
                          -OutFile $zip -UseBasicParsing
        Expand-Archive -Path $zip -DestinationPath $stage -Force

        # The archive nests everything under Release\. Flatten it: the binaries
        # load their ggml/cudart DLLs from their own directory, so a preserved
        # folder layout gives you an exe that cannot start.
        Get-ChildItem -Path $stage -Recurse -File |
            Move-Item -Destination $whisperDir -Force

        Remove-Item $zip, $stage -Recurse -Force
        if (-not (Test-Path $server)) {
            throw "whisper-server.exe missing after extracting $asset -- delete the whisper folder and re-run."
        }
    } else {
        Write-Host 'have  whisper-server.exe'
    }

    Get-File "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-$Model.bin" `
             (Join-Path $models "ggml-$Model.bin")
}

Write-Host ''
Write-Host 'done. now:'
Write-Host '  $env:ANTHROPIC_API_KEY = "sk-ant-..."'
if ($Whisper) {
    Write-Host ''
    Write-Host '  # offline STT -- leave this running in its own window:'
    Write-Host "  .\whisper\whisper-server.exe -m .\models\ggml-$Model.bin --host 127.0.0.1 --port 8231"
    Write-Host '  $env:IRA_STT_URL = "http://127.0.0.1:8231/inference"'
    Write-Host ''
} else {
    Write-Host '  $env:GROQ_API_KEY = "gsk_..."'
}
Write-Host '  cargo run --release'
