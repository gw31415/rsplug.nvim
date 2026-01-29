--- mode() 準拠文字列に従いマップするモード文字を返す。
---@param mode string
---@return string[]
local function parse_mode(mode)
	---@param prefix string
	---@return boolean
	local function startswith(prefix)
		return mode:find(prefix, 1, true) == 1
	end

	if startswith 'no' then
		return { 'o', '' }
	elseif startswith 'n' then
		return { 'n', '' }
	end
	if startswith 'v' or startswith 'V' or startswith '' then
		return { 'x', 'v', '' }
	end
	if startswith 's' or startswith 'S' or startswith '' then
		return { 's', 'v' }
	end
	if startswith 'i' or startswith 'R' then
		return { 'i' }
	end
	if startswith 'c' then
		return { 'c' }
	end
	if startswith 't' then
		return { 't' }
	end
	return {}
end

local setup_done = {}

return {
	---@param mode string
	setup = function(mode)
		for _, mode_char in ipairs(parse_mode(mode)) do
			if not setup_done[mode_char] then
				setup_done[mode_char] = true
				local exists, mod = pcall(require, '_rsplug/on_map/mode_' .. mode_char)
				for pattern, ids in pairs(exists and mod or {}) do
					vim.keymap.set(mode_char, pattern, function()
						vim.keymap.del(mode_char, pattern, {})
						for _, id in ipairs(ids) do
							require '_rsplug'.packadd(id)
						end
						vim.api.nvim_feedkeys(
							vim.api.nvim_replace_termcodes(pattern, true, false, true),
							'imt',
							true
						)
						return ''
					end, { expr = true, silent = true })
				end
			end
		end
	end,
}
