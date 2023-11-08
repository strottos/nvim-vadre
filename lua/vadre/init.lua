if vim.g.loaded_nvim_ladder == 1 then
	return
end

vim.g.loaded_nvim_ladder = 1
vim.g.vadre_last_command = {}
vim.g.vadre_single_thread_mode = 0

require('vadre.setup')
require('vadre.ui')
