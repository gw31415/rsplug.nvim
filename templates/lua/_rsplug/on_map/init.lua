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
-- Track which patterns are associated with each plugin ID
-- id_patterns[id] = { pattern1, pattern2, ... }
local id_patterns = {}
-- Track which plugin IDs have been loaded
local loaded_plugins = {}

return {
	---@param mode string
	setup = function(mode)
		for _, mode_char in ipairs(parse_mode(mode)) do
			if not setup_done[mode_char] then
				setup_done[mode_char] = true
				local exists, mod = pcall(require, '_rsplug/on_map/mode_' .. mode_char)
				for pattern, ids in pairs(exists and mod or {}) do
					-- Check if all plugins for this pattern are already loaded
					local all_loaded = true
					for _, id in ipairs(ids) do
						if not loaded_plugins[id] then
							all_loaded = false
							break
						end
					end
					-- Skip setup if plugin is already loaded (real mappings exist)
					if all_loaded then
						goto continue
					end

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
						-- Track reverse mapping: plugin ID to patterns
						if not id_patterns[id] then
							id_patterns[id] = {}
						end
						local pattern_found = false
						for _, existing_pattern in ipairs(id_patterns[id]) do
							if existing_pattern == pattern then
								pattern_found = true
								break
							end
						end
						if not pattern_found then
							table.insert(id_patterns[id], pattern)
						end
					end

					vim.keymap.set(mode_char, pattern, function()
						-- Get all plugin IDs for this pattern
						local all_ids = pattern_ids[pattern] or ids

						-- Check if all plugins are already loaded
						local all_already_loaded = true
						for _, id in ipairs(all_ids) do
							if not loaded_plugins[id] then
								all_already_loaded = false
								break
							end
						end

						-- If plugin is already loaded, delete only this mapping and feed keys
						if all_already_loaded then
							pcall(vim.keymap.del, mode_char, pattern, {})
							vim.api.nvim_feedkeys(
								vim.api.nvim_replace_termcodes(pattern, true, false, true),
								'imt',
								true
							)
							return ''
						end

						-- Collect all patterns that need to be deleted
						-- (all patterns associated with the plugins being loaded)
						local patterns_to_delete = {}
						for _, id in ipairs(all_ids) do
							local related_patterns = id_patterns[id] or {}
							for _, related_pattern in ipairs(related_patterns) do
								patterns_to_delete[related_pattern] = true
							end
						end

						-- Delete all related pattern mappings in all their modes
						for pattern_to_delete, _ in pairs(patterns_to_delete) do
							local modes = pattern_modes[pattern_to_delete] or {}
							for _, m in ipairs(modes) do
								pcall(vim.keymap.del, m, pattern_to_delete, {})
							end
							-- Clear tracking for this pattern
							pattern_modes[pattern_to_delete] = nil
							pattern_ids[pattern_to_delete] = nil
						end

						-- Load all plugins that registered this pattern
						for _, id in ipairs(all_ids) do
							require '_rsplug'.packadd(id)
							-- Mark plugin as loaded
							loaded_plugins[id] = true
							-- Clear tracking for this plugin ID
							id_patterns[id] = nil
						end

						vim.api.nvim_feedkeys(
							vim.api.nvim_replace_termcodes(pattern, true, false, true),
							'imt',
							true
						)
						return ''
					end, { expr = true, silent = true })

					::continue::
				end
			end
		end
	end,
}
