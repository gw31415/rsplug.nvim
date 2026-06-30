vim.api.nvim_create_autocmd('ModeChanged', {
	callback = function() require '_rsplug/on_map'.setup(vim.v.event['new_mode']) end
})
vim.api.nvim_create_autocmd('VimEnter', {
	callback = function()
		local on_map = require '_rsplug/on_map'
		on_map.setup 'n'
		on_map.setup 'no'
	end
})
