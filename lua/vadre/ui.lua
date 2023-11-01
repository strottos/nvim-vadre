local Input = require("nui.input")
local NuiText = require("nui.text")
local Menu = require("nui.menu")
local Popup = require("nui.popup")
local event = require("nui.utils.autocmd").event

local debuggers = {
    {
        name = "CodeLLDB",
        desc = "A debugger for LLDB, for debugging Rust and C/C++ amongst other languages",
        short = "codelldb",
    },
    {
        name = "Go Delve",
        desc = "A debugger for Go programs",
        short = "delve",
    },
}

local function submit_debugger(item)
    local popup_options = {
        relative = "editor",
        position = "50%",
        size = "100%",
        border = {
            style = "rounded",
            text = {
                top = " Enter Command to Debug: ",
                top_align = "left",
            },
        },
        win_options = {
            winhighlight = "Normal:Normal",
        },
    }

    local debugger_type = debuggers[item.idx].short

    local input = Input(popup_options, {
        prompt = "> ",
        default_value = vim.g.vadre_debugger_default_command and vim.g.vadre_debugger_default_command[debugger_type]
            or vim.g.vadre_last_command and vim.g.vadre_last_command[debugger_type] or "",
        on_submit = function(cmd)
            vim.cmd("VadreDebug -t=" .. debugger_type .. " -- " .. cmd)
        end,
    })

    input:mount()

    -- unmount component when cursor leaves buffer
    input:on(event.BufLeave, function()
        input:unmount()
    end)
end

local function start_debugger_ui()
    local entries = {}

    local max_width = 30

    for idx, debugger in pairs(debuggers) do
        local line = "  " .. debugger.name
        if debugger.desc ~= nil then
            line = line .. " - " .. debugger.desc
        end
        line = line .. "  "
        entries[idx] = Menu.item(NuiText(line), { idx = idx })
        if #line > max_width then
            max_width = #line
        end
    end

    local title = NuiText("  Choose a Debugger:  ")
    local max_height = math.max(#entries, 10)
    local width = math.min(max_width, vim.api.nvim_win_get_width(0) - 10)
    local height = math.min(max_height, vim.api.nvim_win_get_height(0) - 10)

    local popup = Menu({
        position = "50%",
        size = {
            width = width,
            height = height,
        },
        border = {
            style = "single",
            text = {
                top = title,
                top_align = "center",
            },
        },
        relative = "editor",
        win_options = {
            winhighlight = "Normal:Normal,FloatBorder:Normal",
        },
    }, {
        lines = entries,
        on_submit = submit_debugger,
    })
    popup:mount()

    popup:on(event.BufLeave, function()
        popup:unmount()
    end)
end

local logs_window_popup
local callstack_window_popup
local variables_window_popup
local breakpoints_window_popup

local function capitalize(str)
    return (str:gsub("^%l", string.upper))
end

local function set_popup(type, bufnr)
    local new_popup = Popup({
        enter = false,
        focusable = true,
        bufnr = bufnr,
        border = {
            style = "rounded",
            text = {
                top = " " .. capitalize(type) .. " ",
            }
        },
        relative = {
            type = "buf",
            position = {
                row = 0,
                col = 0,
            }
        },
        position = {
            row = 0,
            col = 0,
        },
        size = {
            width = "100%",
            height = "100%",
        },
        win_options = {
            winhighlight = "Normal:Normal,FloatBorder:Normal",
        },
    })

    if type == "logs" then
        logs_window_popup = new_popup
    elseif type == "callstack" then
        callstack_window_popup = new_popup
    elseif type == "variables" then
        variables_window_popup = new_popup
    elseif type == "breakpoints" then
        breakpoints_window_popup = new_popup
    else
        vim.notify("Unknown output type: " .. type, vim.log.levels.CRITICAL)
    end
end

local function display_output_window(type)
    if type == "logs" then
        logs_window_popup:mount()
    elseif type == "callstack" then
        callstack_window_popup:mount()
    elseif type == "variables" then
        variables_window_popup:mount()
    elseif type == "breakpoints" then
        breakpoints_window_popup:mount()
    elseif type == "terminal" then
    else
        vim.notify("Unknown output type: " .. type, vim.log.levels.CRITICAL)
    end
end

local function hide_output_window(type)
    if type == "logs" then
        logs_window_popup:unmount()
    elseif type == "callstack" then
        callstack_window_popup:unmount()
    elseif type == "variables" then
        variables_window_popup:unmount()
    elseif type == "breakpoints" then
        breakpoints_window_popup:unmount()
    elseif type == "terminal" then
    else
        vim.notify("Unknown output type: " .. type, vim.log.levels.CRITICAL)
    end
end

local M = {
    start_debugger_ui = start_debugger_ui,
    set_popup = set_popup,
    display_output_window = display_output_window,
    hide_output_window = hide_output_window,
}

return M
