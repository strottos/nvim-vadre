#!/bin/bash

vadre_root=$(dirname "$(realpath $0)")

function setup_environment {
    local plugins_dir="${vadre_root}/test_files/site/pack/vendor/start"

    echo "Installing plugins to ${plugins_dir}"

    if [[ ! -d "${plugins_dir}" ]]; then
        mkdir -p "${plugins_dir}"
    fi

    if [[ ! -d "${plugins_dir}/plenary.nvim" ]]; then
        echo "Installing plenary.nvim to ${plugins_dir}/plenary.nvim"
        git clone https://github.com/nvim-lua/plenary.nvim "${plugins_dir}/plenary.nvim"
        echo "Installed plenary.nvim"
    else
        echo "plenary.nvim already installed, updating"
        pushd "${plugins_dir}/plenary.nvim"
        git pull
        popd
    fi

    if [[ ! -d "${plugins_dir}/nui.nvim" ]]; then
        echo "Installing nui.nvim to ${plugins_dir}/nui.nvim"
        git clone https://github.com/MunifTanjim/nui.nvim "${plugins_dir}/nui.nvim"
        echo "Installed nui.nvim"
    else
        echo "nui.nvim already installed, updating"
        pushd "${plugins_dir}/nui.nvim"
        git pull
        popd
    fi

    echo "All plugins installed, doing remainder of setup"

    # If logging enabled spit the neovim logs out
    export NVIM_LOG_FILE="$(pwd)/nvim_log"
    export VADRE_LOG="trace"
    export VADRE_LOG_FILE="$(pwd)/vadre_log"

    echo "Done setup"
}

function run_nvim {
    cargo build --manifest-path="${vadre_root}/Cargo.toml" && nvim --clean -u "${vadre_root}/test_files/vimfiles/mininit.lua" ${@}
}

setup_environment
run_nvim ${@}
