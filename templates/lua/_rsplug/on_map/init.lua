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
-- Track which modes have which patterns set up
-- pattern_modes[pattern] = { mode1, mode2, ... }
local pattern_modes = {}
-- Track all plugin IDs for each pattern across all modes
-- pattern_ids[pattern] = { id1, id2, ... }
local pattern_ids = {}

return {
	---@param mode string
	setup = function(mode)
		for _, mode_char in ipairs(parse_mode(mode)) do
			if not setup_done[mode_char] then
				setup_done[mode_char] = true
				local exists, mod = pcall(require, '_rsplug/on_map/mode_' .. mode_char)
				for pattern, ids in pairs(exists and mod or {}) do
					-- Track that this pattern is set up in this mode
					if not pattern_modes[pattern] then
						pattern_modes[pattern] = {}
						pattern_ids[pattern] = {}
					end
					table.insert(pattern_modes[pattern], mode_char)
					-- Collect all unique plugin IDs for this pattern
					for _, id in ipairs(ids) do
						local found = false
						for _, existing_id in ipairs(pattern_ids[pattern]) do
							if existing_id == id then
								found = true
								break
							end
						end
						if not found then
							table.insert(pattern_ids[pattern], id)
						end
					end

					vim.keymap.set(mode_char, pattern, function()
						-- Delete the mapping in ALL modes where it was set up
						local modes = pattern_modes[pattern] or { mode_char }
						for _, m in ipairs(modes) do
							pcall(vim.keymap.del, m, pattern, {})
						end

						-- Load all plugins that registered this pattern in any mode
						local all_ids = pattern_ids[pattern] or ids
						for _, id in ipairs(all_ids) do
							require '_rsplug'.packadd(id)
						end

						-- Clear the tracking for this pattern
						pattern_modes[pattern] = nil
						pattern_ids[pattern] = nil

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
