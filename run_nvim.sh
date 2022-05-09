#!/bin/bash
vadre_root=$(dirname "$(realpath $0)")
cargo build && nvim -u "${vadre_root}/test/vimfiles/vimrc"
