local ui = require('vadre.ui')

function dump(o)
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

local function on_close_vadre_window(opts)
    local instance_id = string.match(opts.file, "Vadre Code .(%d).") or string.match(opts.file, "Vadre Program .(%d).")
    ui.hide_output_window(tonumber(instance_id))
    vim.api.nvim_command("VadreStopDebugger " .. instance_id)
    vim.api.nvim_del_augroup_by_name("vadre_" .. instance_id)
end

local function on_enter_vadre_output_window(opts)
    local instance_id = string.match(opts.file, "Vadre Code .(%d).") or string.match(opts.file, "Vadre Program .(%d).")
    if ui.get_current_output_window(tonumber(instance_id)) ~= "terminal" then
        vim.api.nvim_command("wincmd w")
    end
end

return {
    on_close_vadre_window = on_close_vadre_window,
    on_enter_vadre_output_window = on_enter_vadre_output_window,
}
