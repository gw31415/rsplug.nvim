vim.api.nvim_create_autocmd('ModeChanged', {
	callback = function() require '_rsplug/on_map'.setup(vim.v.event['new_mode']) end
})
local ns_id = nil
ns_id = vim.on_key(function()
	vim.on_key(nil, ns_id)
	require '_rsplug/on_map'.setup 'n'
end)
