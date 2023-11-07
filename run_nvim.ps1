$vadre_root=$PSScriptRoot

function setup_environment {
    $plugins_dir = "${vadre_root}/test_files/site/pack/plugins/start"

    Write-Output "Installing plugins to dir ${plugins_dir}"

    if (-not (Test-Path $plugins_dir)) {
        New-Item -ItemType Directory -Force -Path $plugins_dir
    }

    if (-not (Test-Path $plugins_dir/plenary.nvim)) {
        Write-Output "Installing plenary.nvim to ${plugins_dir}/plenary.nvim"
        git clone https://github.com/nvim-lua/plenary.nvim "${plugins_dir}/plenary.nvim"
        Write-Output "Installed plenary.nvim"
    } else {
        Write-Output "plenary.nvim already installed, updating"
        Push-Location "${plugins_dir}/plenary.nvim"
        git pull
        Pop-Location
    }

    if (-not (Test-Path $plugins_dir/nui.nvim)) {
        Write-Output "Installing nui.nvim to ${plugins_dir}/plenary.nvim"
        git clone https://github.com/nvim-lua/nui.nvim "${plugins_dir}/plenary.nvim"
        Write-Output "Installed nui.nvim"
    } else {
        Write-Output "nui.nvim already installed, updating"
        Push-Location "${plugins_dir}/nui.nvim"
        git pull
        Pop-Location
    }

    Write-Output "All plugins installed, doing remainder of setup"

    # If logging enabled spit the neovim logs out
    $env:NVIM_LOG_FILE="$(Get-Location)/nvim_log"
    $env:VADRE_LOG="trace"
    $env:VADRE_LOG_FILE="$(Get-Location)/vadre_log"

    Write-Output "Done setup"
}

function run_nvim {
    cargo build --manifest-path="${vadre_root}/Cargo.toml"

    if ($LastExitCode -ne 0) {
        return
    }

    nvim --clean -u "${vadre_root}/test_files/vimfiles/mininit.lua" @args
}

setup_environment
run_nvim @args
