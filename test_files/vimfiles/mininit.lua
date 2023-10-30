-- Need the absolute path as when doing the testing we will issue things like `tcd` to change directory
-- to where our temporary filesystem lives
local root_dir = vim.fn.expand('<sfile>:p:h:h:h') .. "/"

package.path = string.format("%s;%s?.lua;%s?/init.lua", package.path, root_dir, root_dir)

vim.opt.packpath:prepend(string.format("%s", root_dir .. "test_files/site"))

vim.opt.rtp = {
	root_dir,
	vim.env.VIMRUNTIME,
}

vim.cmd([[
  filetype on
  packadd plenary.nvim
  packadd nui.nvim
]])

vim.bo.swapfile = false
vim.o.tabstop = 4
vim.o.softtabstop = vim.o.tabstop
vim.o.shiftwidth = vim.o.tabstop
vim.o.expandtab = true

vim.g.vadre_log_level=5
vim.g.maplocalleader='-'
-- nnoremap <silent> <localleader>b <cmd>VadreBreakpoint<CR>
