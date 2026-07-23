--- mode() 準拠文字列に従いマップするモード文字を返す。
---@param mode string
---@return string[]
local function parse_mode(mode)
	local first = mode:sub(1, 1)
	if mode:sub(1, 2) == 'no' then
		return { 'o', '' }
	elseif first == 'n' then
		return { 'n', '' }
	end
	if first == 'v' or first == 'V' or first == '' then
		return { 'x', 'v', '' }
	end
	if first == 's' or first == 'S' or first == '' then
		return { 's', 'v' }
	end
	if first == 'i' or first == 'R' then
		return { 'i' }
	end
	if first == 'c' then
		return { 'c' }
	end
	if first == 't' then
		return { 't' }
	end
	return {}
end

local setup_done = {}
local rsplug = require '_rsplug'

local function all_loaded(ids)
	for _, id in ipairs(ids) do
		if not rsplug.loaded[id] then
			return false
		end
	end
	return true
end
-- Track which modes have which patterns set up
-- pattern_modes[pattern] = { mode1, mode2, ... }
local pattern_modes = {}
-- Track all plugin IDs for each pattern across all modes
-- pattern_ids[pattern] = { id1, id2, ... }
local pattern_ids = {}
local pattern_id_set = {}
-- Track which patterns are associated with each plugin ID
-- id_patterns[id] = { [pattern] = true, ... }
local id_patterns = {}
local id_pattern_order = {}

---Retire one package from only the patterns registered by that package.
local function retire(id)
	for _, pattern in ipairs(id_pattern_order[id] or {}) do
		local ids = pattern_ids[pattern]
		if ids then
			for i = #ids, 1, -1 do
				if ids[i] == id then table.remove(ids, i) end
			end
			if #ids == 0 then
				for _, mode in ipairs(pattern_modes[pattern] or {}) do
					pcall(vim.keymap.del, mode, pattern, {})
				end
				pattern_modes[pattern] = nil
				pattern_ids[pattern] = nil
				pattern_id_set[pattern] = nil
			end
		end
	end
	id_patterns[id] = nil
	id_pattern_order[id] = nil
end

rsplug.on_loaded(retire)

local M = {}
-- 到達可能モードのうち未 setup のもの。plugin/on_map.stpl が設定する。
M.pending_modes = {}

---pattern 関連の追跡を一括クリアする（pattern_modes / pattern_ids / 当該 pattern を含む id_patterns エントリ）。
local function remove_pattern(pattern)
	for _, id in ipairs(pattern_ids[pattern] or {}) do
		local patterns = id_patterns[id]
		if patterns then
			patterns[pattern] = nil
			if next(patterns) == nil then
				id_patterns[id] = nil
				id_pattern_order[id] = nil
			end
		end
	end
	pattern_modes[pattern] = nil
	pattern_ids[pattern] = nil
	pattern_id_set[pattern] = nil
end

---全 pending モードが setup されたら watcher 用 augroup を削除する。pcall ガード・冪等。
function M.retire()
	if M._retired then
		return
	end
	if next(M.pending_modes) ~= nil then
		return
	end
	M._retired = true
	-- nvim_del_augroup は存在しないことがあるため by_name を使う。
	pcall(vim.api.nvim_del_augroup_by_name, 'rsplug.runtime.on_map')
end

---@param mode string
function M.setup(mode)
	for _, mode_char in ipairs(parse_mode(mode)) do
		-- 到達可能かつ未 setup かつ pending のモードだけ処理する。
		if not setup_done[mode_char] and M.pending_modes[mode_char] then
			setup_done[mode_char] = true
			M.pending_modes[mode_char] = nil
			local exists, mod = pcall(require, '_rsplug/on_map/mode_' .. mode_char)
			for _, record in ipairs(exists and mod or {}) do
				local pattern = record.pattern
				local ids = record.ids
				-- Skip setup if plugin is already loaded (real mappings exist)
				if all_loaded(ids) then
					goto continue
				end

				-- Track that this pattern is set up in this mode
				if not pattern_modes[pattern] then
					pattern_modes[pattern] = {}
					pattern_ids[pattern] = {}
					pattern_id_set[pattern] = {}
				end
				table.insert(pattern_modes[pattern], mode_char)
				-- Collect all unique plugin IDs for this pattern
				for _, id in ipairs(ids) do
					if not pattern_id_set[pattern][id] then
						pattern_id_set[pattern][id] = true
						table.insert(pattern_ids[pattern], id)
					end
					-- Track reverse mapping: plugin ID to patterns
					if not id_patterns[id] then
						id_patterns[id] = {}
						id_pattern_order[id] = {}
					end
					if not id_patterns[id][pattern] then
						id_patterns[id][pattern] = true
						table.insert(id_pattern_order[id], pattern)
					end
				end

				local replay = vim.api.nvim_replace_termcodes(pattern, true, false, true)
				vim.keymap.set(mode_char, pattern, function()
					-- Get all plugin IDs for this pattern
					local all_ids = pattern_ids[pattern] or ids

					-- If plugin is already loaded, delete only this mapping and feed keys
					if all_loaded(all_ids) then
						pcall(vim.keymap.del, mode_char, pattern, {})
						vim.api.nvim_feedkeys(replay, 'imt', true)
						return ''
					end

					-- Collect all patterns that need to be deleted
					-- (all patterns associated with the plugins being loaded)
					local patterns_to_delete = {}
					for _, id in ipairs(all_ids) do
						for _, related_pattern in ipairs(id_pattern_order[id] or {}) do
							patterns_to_delete[related_pattern] = true
						end
					end

					-- Delete all related pattern mappings in all their modes
					for pattern_to_delete, _ in pairs(patterns_to_delete) do
						local modes = pattern_modes[pattern_to_delete] or {}
						for _, m in ipairs(modes) do
							pcall(vim.keymap.del, m, pattern_to_delete, {})
						end
						-- Clear tracking for this pattern (centralized removal)
						remove_pattern(pattern_to_delete)
					end

					-- Load all plugins that registered this pattern
					for _, id in ipairs(all_ids) do
						rsplug.packadd(id)
					end

					vim.api.nvim_feedkeys(replay, 'imt', true)
					return ''
				end, { expr = true, silent = true })

				::continue::
			end
			M.retire()
		end
	end
end

---ModeChanged コールバック。new_mode を parse し、pending モードと一致したときだけ
---setup する。一致が無ければモジュールを require せず復帰する（全モード走査しない）。
---@param new_mode string
function M.on_mode_changed(new_mode)
	local chars = parse_mode(new_mode)
	local hit = false
	for _, c in ipairs(chars) do
		if M.pending_modes[c] then
			hit = true
			break
		end
	end
	if not hit then
		return
	end
	M.setup(new_mode)
end

return M
