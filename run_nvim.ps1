$vadre_root=$PSScriptRoot

function setup_environment {
}

function run_nvim {
    cargo build --manifest-path="${vadre_root}/Cargo.toml"

    if ($LastExitCode -ne 0) {
        return
    }

    $env:NVIM_LOG_FILE="$(Get-Location)/nvim_log"
    $env:VADRE_LOG='trace'
    $env:VADRE_LOG_FILE="$(Get-Location)/vadre_log"

    echo nvim --clean -u "${vadre_root}/test_files/vimfiles/mininit.lua" ${args}
}

setup_environment
run_nvim
