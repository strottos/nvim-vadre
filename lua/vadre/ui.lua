local Input = require("nui.input")
local NuiText = require("nui.text")
local Menu = require("nui.menu")
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

local M = {
    start_debugger_ui = start_debugger_ui,
}

return M