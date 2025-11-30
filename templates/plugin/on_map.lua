vim.api.nvim_create_autocmd('ModeChanged', {
	callback = function() require '_rsplug/on_map'.setup(vim.v.event['new_mode']) end
})
local ns_id = nil
ns_id = vim.on_key(function(_key, typed)
	vim.on_key(nil, ns_id)
	require '_rsplug/on_map'.setup 'n'
	vim.schedule(function()
		vim.api.nvim_feedkeys(typed, 'm', true)
	end)
	return ''
end)
