cargo build
if ($LastExitCode -ne 0) {
    return
}
nvim -u "${PSScriptRoot}/test/vimfiles/vimrc"
