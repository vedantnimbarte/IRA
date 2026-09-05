# Downloads everything IRA needs into .\models and .\piper
# Run from the repo root:  .\scripts\fetch-models.ps1

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

Write-Host ''
Write-Host 'done. now:'
Write-Host '  $env:ANTHROPIC_API_KEY = "sk-ant-..."'
Write-Host '  $env:GROQ_API_KEY = "gsk_..."'
Write-Host '  cargo run --release'
