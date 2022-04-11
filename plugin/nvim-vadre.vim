" VIM VADRE Plugin

if exists("g:loaded_vadre_plugin")
    finish
endif

" Set a special flag used only by this plugin for preventing doubly
" loading the script.
let g:loaded_vadre_plugin = 1

" Initialize the channel
if !exists("s:vadreJobId")
    let s:vadreJobId = 0
endif

let s:vadrePluginRoot = expand("<sfile>:p:h:h")
let s:vadreBinary = ""

for rust_type in ["release", "debug"]
    let program_name = "nvim-vadre"
    if has("win32")
        let program_name = program_name . ".exe"
    endif

    let full_path = s:vadrePluginRoot . "/target/" . rust_type . "/" . program_name

    if filereadable(full_path)
        let s:vadreBinary = full_path
        break
    endif
endfor

if s:vadreBinary == ""
    echoerr 'VADRE program not found, please install rust and run `cargo build` in the VADRE plugin directory: ' . s:vadrePluginRoot
    finish
endif

" Entry point. Initialize RPC. If it succeeds, then attach commands to the `rpcnotify` invocations.
function! s:connect()
    let id = s:initRpc()

    if 0 == id
        echoerr "VADRE: cannot start rpc process"
    elseif -1 == id
        echoerr "VADRE: rpc process is not executable"
    else
        " Mutate our jobId variable to hold the channel ID
        let s:vadreJobId = id

        call s:configureCommands()
    endif
endfunction

function! s:configureCommands()
    command! VadrePing :call s:ping()
endfunction

function! s:ping()
    echom rpcrequest(s:vadreJobId, "ping")
endfunction

" Initialize RPC
function! s:initRpc()
    if s:vadreJobId == 0
        let jobid = jobstart([s:vadreBinary], { "rpc": v:true })
        return jobid
    else
        return s:vadreJobId
    endif
endfunction

call s:connect()
