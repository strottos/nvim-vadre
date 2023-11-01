if vim.g.loaded_nvim_ladder == 1 then
	return
end

vim.g.loaded_nvim_ladder = 1
vim.g.vadre_last_command = {}

require('vadre.setup')
require('vadre.ui')
