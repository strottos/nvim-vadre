-- Initialize the channel
local vadre_job_id = 0

local plugin_root_dir = vim.fn.expand('<sfile>:p:h:h')
local plugin_binary = ''

for _, rust_target in ipairs({'release', 'debug'}) do
    local program_name = 'nvim-vadre'
    if vim.api.nvim_call_function('has', {'win32'}) == 1 then
        program_name = program_name .. '.exe'
    end

    local full_path = plugin_root_dir .. "/target/" .. rust_target .. "/" .. program_name

    if vim.api.nvim_call_function('filereadable', {full_path}) == 1 then
        plugin_binary = full_path
        break
    end
end

if plugin_binary == "" then
    vim.api.nvim_err_writeln("Could not find nvim-vadre binary, not proceeding with setup, please install Rust and run `cargo build` in the directory")
    return
end

local function dump(o)
   if type(o) == 'table' then
      local s = '{ '
      for k,v in pairs(o) do
         if type(k) ~= 'number' then k = '"'..k..'"' end
         s = s .. '['..k..'] = ' .. dump(v) .. ','
      end
      return s .. '} '
   else
      return tostring(o)
   end
end

local jobid = vim.fn.jobstart({plugin_binary}, {
  rpc = true,
  -- Vadre is intended to not spit something to stderr, we log it as critical if it does
  on_stderr = function(_, data, _)
    vim.api.nvim_err_writeln('Vadre stderr: ' .. dump(data))
  end,
  -- Stdout is handled by rpcrequest
  on_exit = function()
    vim.api.nvim_err_writeln("vadre exited")
  end,
})

vim.api.nvim_command('autocmd VimLeave * call jobstop(' .. jobid .. ')')

if jobid == 0 or jobid == -1 then
    vim.api.nvim_err_writeln("Could not start nvim-vadre, not proceeding with setup")
    return
end

vim.api.nvim_create_user_command("VadrePing", function(opts)
    print(vim.rpcrequest(jobid, "ping", {}))
end, {})
vim.api.nvim_create_user_command("VadreDebug", function(opts)
    print(vim.rpcrequest(jobid, "launch", opts.fargs))
end, {complete = "file", nargs = "*"})
vim.api.nvim_create_user_command("VadreBreakpoint", function(opts)
    print(vim.rpcrequest(jobid, "breakpoint", {}))
end, {nargs = 0})
vim.api.nvim_create_user_command("VadreStepIn", function(opts)
    print(vim.rpcrequest(jobid, "step_in", {opts.fargs[1], opts.count}))
end, {nargs = 1, count = 1})
vim.api.nvim_create_user_command("VadreStepOver", function(opts)
    print(vim.rpcrequest(jobid, "step_over", {opts.fargs[1], opts.count}))
end, {nargs = 1, count = 1})
vim.api.nvim_create_user_command("VadreContinue", function(opts)
    print(vim.rpcrequest(jobid, "continue", opts.fargs[1]))
end, {nargs = 1})
vim.api.nvim_create_user_command("VadreStopDebugger", function(opts)
    print(vim.rpcrequest(jobid, "stop_debugger", opts.fargs[1]))
end, {nargs = 1})
vim.api.nvim_create_user_command("VadreLogMsg", function(opts)
    print(vim.rpcrequest(jobid, "log_msg", opts.fargs))
end, {nargs = "*"})
vim.api.nvim_create_user_command("VadreOutputWindow", function(opts)
    print(vim.rpcrequest(jobid, "output_window", opts.fargs))
end, {nargs = "*"})
