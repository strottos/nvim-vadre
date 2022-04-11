#!/bin/bash
vadre_root=$(dirname "$(realpath $0)")
nvim -u "${vadre_root}/test/vimfiles/vimrc"
