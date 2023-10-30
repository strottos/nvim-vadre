if (Test-Path 'Cargo.toml') {
    cargo build
    if ($LastExitCode -ne 0) {
        return
    }
}
vadre_root=$PSScriptRoot
cargo build --manifest-path="${vadre_root}/Cargo.toml"

$env:NVIM_LOG_FILE="$(Get-Location)/nvim_log"
$env:VADRE_LOG='trace'
$env:VADRE_LOG_FILE="$(Get-Location)/vadre_log"

nvim --clean -u "${PSScriptRoot}/test_files/vimfiles/vimrc"
