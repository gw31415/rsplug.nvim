-- Runtime hot-paths characterization driver (PLANS R0).
--
-- Rust 側テスト（lazy_registration.rs::runtime_hot_paths）が nvim を
--   nvim --headless -u NONE -i NONE -n -c 'luafile <this>' -c 'qa!'
-- で起動する。環境変数:
--   RSPLUG_TEST_PACKPATH  : pack ルート（init.lua, pack/_gen を含む）
--   RSPLUG_TEST_SCENARIO  : 実行するシナリオ名
--   RSPLUG_TEST_EXPECT    : truthy でなければならない vim.g タグのカンマ区切り
--   RSPLUG_TEST_NVIM_RUNS : (任意) 1 にすると Victoria gate 用の計測ヘルパを露出
--
-- 成功時は正確に1行  `RSPLUG_TEST_RESULT=ok` を出す。失敗時は
-- `RSPLUG_TEST_RESULT=fail: <reason>`。計測値は `RSPLUG_TEST_COUNT <name>=<n>` 行。
--
-- 注意: rsplug の生成コードが呼ぶ `vim.api.nvim_*` を計測するため、boot 前に
-- 該当 API を計数ラッパへ差し替える（nvim 0.12 では vim.api のフィールド書き換え可）。

local counts = {}
local counting = true
local function wrap(name)
	local orig = vim.api[name]
	if type(orig) == 'function' then
		vim.api[name] = function(...)
			if counting then
				counts[name] = (counts[name] or 0) + 1
			end
			return orig(...)
		end
	end
end
-- ft/event ホットパスで問題となる API。追加は各フェーズで必要に応じて行う。
wrap 'nvim_get_runtime_file'
wrap 'nvim_get_autocmds'

local function snapshot()
	local s = {}
	for k, v in pairs(counts) do
		s[k] = v
	end
	return s
end
local function emit_delta(before)
	for k, v in pairs(counts) do
		local d = v - (before[k] or 0)
		if d ~= 0 then
			print(string.format('RSPLUG_TEST_COUNT %s=%d', k, d))
		end
	end
end

-- vim.g[expect] が全て truthy なら nil、そうでなければ失敗理由を返す。
local function check_expect()
	local exp = os.getenv 'RSPLUG_TEST_EXPECT' or ''
	for tag in string.gmatch(exp, '[^,]+') do
		if not vim.g[tag] then
			return ('expected vim.g.%s to be truthy, got %s'):format(tag, vim.inspect(vim.g[tag]))
		end
	end
	return nil
end

-- pack の init.lua（generation ローダ）を source して rsplug ランタイムを起動。
local function boot()
	local packpath = assert(os.getenv 'RSPLUG_TEST_PACKPATH', 'RSPLUG_TEST_PACKPATH not set')
	vim.opt.packpath:prepend(packpath)
	-- 実運用では init.lua は通常起動の vimrc として（loadplugins=true で）source される。
	-- テストは -u NONE で隔離するため loadplugins が off になる。これを本来の状態に戻す。
	vim.o.loadplugins = true
	vim.cmd('source ' .. vim.fn.fnameescape(packpath .. '/init.lua'))
end

-- 既定のスクラッチバッファを current にする（filetype/mapping テスト用）。
local function scratch_buf()
	local b = vim.api.nvim_create_buf(false, true)
	vim.api.nvim_set_current_buf(b)
	return b
end

local scenarios = {}

-- 共有イベント: 2プラグインが同じ User イベントに登録。1回のトリガで両方読む。
scenarios.shared_events = function()
	boot()
	vim.api.nvim_exec_autocmds('User', { pattern = 'R0Shared', modeline = false })
	return check_expect()
end

-- R2: index_autocmds / new_autocmds の純粋論理検証。
-- 100件の旧 autocmd（うち1件は既存グループ）+ 新規グループ/groupless/既存グループ追加/rsplugグループ。
scenarios.autocmd_diff_helpers = function()
	boot()
	local rsplug = require '_rsplug'
	local excluded = { ['rsplug.runtime.on_event'] = true }
	local before_items = {}
	for i = 1, 99 do
		before_items[i] = { id = i, event = 'Foo' }
	end
	before_items[100] = { id = 100, event = 'Foo', group = 'preexist' }
	local before = rsplug.index_autocmds(before_items, excluded)
	if before.groups['preexist'] ~= true then
		return 'before.groups should contain preexist'
	end
	if before.groups['newgrp'] ~= nil then
		return 'before.groups should not contain newgrp'
	end
	if before.by_id[1] ~= true or before.by_id[100] ~= true then
		return 'before.by_id missing old ids'
	end
	local after = {}
	for i = 1, 100 do
		after[#after + 1] = before_items[i]
	end
	after[#after + 1] = { id = 101, event = 'Foo', group = 'newgrp' }
	after[#after + 1] = { id = 102, event = 'Foo' } -- groupless new
	after[#after + 1] = { id = 103, event = 'Foo', group = 'preexist' } -- 既存グループ追加
	after[#after + 1] = { id = 104, event = 'Foo', group = 'rsplug.runtime.on_event' } -- rsplug
	local new = rsplug.new_autocmds(after, before, excluded)
	local ids = {}
	for _, a in ipairs(new) do
		ids[a.id] = true
	end
	-- 101(新規grp), 102(groupless), 103(既存grp追加) は残る。104(rsplug)・旧(1..100)は除外。
	if ids[101] ~= true or ids[102] ~= true or ids[103] ~= true then
		return 'new_autocmds dropped a new autocmd: ' .. vim.inspect(ids)
	end
	if ids[104] == true then
		return 'new_autocmds must exclude rsplug group'
	end
	if ids[1] == true or ids[100] == true then
		return 'new_autocmds must exclude old ids'
	end
	-- 新規グループは newgrp のみ（preexist は before に存在）。
	local new_groups = {}
	for _, a in ipairs(new) do
		if a.group ~= nil and before.groups[a.group] == nil then
			new_groups[a.group] = true
		end
	end
	if new_groups['newgrp'] ~= true or new_groups['preexist'] ~= nil then
		return 'new-group determination wrong: ' .. vim.inspect(new_groups)
	end
	return nil
end

-- R2: loader は発火後に削除され、packadd 中の nested delivery も起きない。
scenarios.loader_removed_no_nested = function()
	boot()
	vim.api.nvim_exec_autocmds('User', { pattern = 'R0Nested', modeline = false })
	local still = false
	for _, a in ipairs(vim.api.nvim_get_autocmds { event = 'User', pattern = 'R0Nested' }) do
		if a.group == 'rsplug.runtime.on_event' then
			still = true
		end
	end
	if still then
		return 'rsplug loader for R0Nested still present after fire'
	end
	local n = vim.g.ev_nested
	if n ~= 1 then
		return ('expected ev_nested==1 (no nested delivery), got ' .. vim.inspect(n))
	end
	return nil
end

-- R3: on_ft で ftplugin ファイルが無いパッケージは packadd だけ行い、source しない。
-- v2 パスなので nvim_get_runtime_file は 0 回。
scenarios.ft_no_match = function()
	boot()
	local b = scratch_buf()
	local before = snapshot()
	vim.bo[b].filetype = 'lua'
	emit_delta(before)
	if not vim.g.nomatch_loaded then
		return 'nomatch package did not load'
	end
	return nil
end

-- R3: 2バッファ目は on_ft が processed で定数時間に早期復帰し、ftplugin は各バッファ
-- 1回ずつ（2バッファ目は nvim の自然な ftplugin source による）。runtime_file は 0。
scenarios.ft_second_buffer = function()
	boot()
	local b1 = scratch_buf()
	vim.bo[b1].filetype = 'lua'
	local n1 = vim.g.sb_count
	if n1 ~= 1 then
		return ('first buffer should source ftplugin once, got ' .. vim.inspect(n1))
	end
	local b2 = scratch_buf()
	local before = snapshot()
	vim.bo[b2].filetype = 'lua'
	emit_delta(before)
	if vim.g.sb_count ~= 2 then
		return ('second buffer should make sb_count==2, got ' .. vim.inspect(vim.g.sb_count))
	end
	return nil
end

-- R3: 別トリガ(on_event)で先に読み込まれたパッケージは on_ft で ftplugin を二重 source
-- しない（ctl.loaded で早期復帰）。自然な filetype source で1回だけ入る。
scenarios.ft_preloaded = function()
	boot()
	vim.api.nvim_exec_autocmds('User', { pattern = 'R0Pre', modeline = false })
	if not vim.g.pre_pkg then
		return 'preloaded package did not load via event'
	end
	if vim.g.pre_ftplugin then
		return 'ftplugin must not be sourced by on_event'
	end
	local b = scratch_buf()
	vim.bo[b].filetype = 'lua'
	if vim.g.pre_ftplugin ~= 1 then
		return ('pre_ftplugin should be 1 (sourced once, not double), got ' .. vim.inspect(vim.g.pre_ftplugin))
	end
	return nil
end

-- R3: 同一 ft に複数 id。全ての ftplugin が source される。
scenarios.ft_multiple_ids = function()
	boot()
	local b = scratch_buf()
	vim.bo[b].filetype = 'lua'
	return check_expect()
end

-- R4: 未登録名の require は状態を肥大させず、全 root 満足後に searcher が削除される。
scenarios.lua_retire_searcher = function()
	boot()
	local state = require '_rsplug/on_lua'
	if state.on_packadd == nil then
		return 'on_packadd not installed'
	end
	local snap = {}
	for k, v in pairs(state.remaining_roots) do
		snap[k] = v
	end
	pcall(require, 'totally.unrelated.xyz')
	pcall(require, 'another.unknown.one')
	for k, v in pairs(state.remaining_roots) do
		if snap[k] ~= v then
			return ('remaining_roots mutated by unrelated require: ' .. k)
		end
	end
	local ok, err = pcall(require, 'mymod')
	if not ok then
		return 'require mymod failed: ' .. tostring(err)
	end
	if not vim.g.lua_root then
		return 'mymod did not load'
	end
	if not vim.wait(50, function()
		return state.on_packadd == nil
	end, 5) then
		return 'searcher not retired after all roots satisfied'
	end
	return nil
end

-- R4: 登録されていないモジュールの require は標準 loader のエラーになる。
scenarios.lua_unknown_module = function()
	boot()
	local ok, err = pcall(require, 'no_such_module_xyz')
	if ok then
		return 'unknown module should error'
	end
	return nil
end

-- R4: 1つの id が複数 root を持つ場合、1回の packadd で全 root が満足する。
scenarios.lua_one_id_multiple_roots = function()
	boot()
	local state = require '_rsplug/on_lua'
	local ok1, a = pcall(require, 'aaa')
	if not ok1 then
		return 'require aaa failed: ' .. tostring(a)
	end
	if not vim.g.aaa_root then
		return 'aaa did not load'
	end
	local ok2, b = pcall(require, 'bbb')
	if not ok2 then
		return 'require bbb failed: ' .. tostring(b)
	end
	if not vim.g.bbb_root then
		return 'bbb did not load'
	end
	if not vim.wait(50, function()
		return state.on_packadd == nil
	end, 5) then
		return 'searcher not retired after one-id-multi-root satisfaction'
	end
	return nil
end

-- R4: 別トリガ(on_event)で先にロード済みの id は、searcher インストール時に
-- reconcile され、require 时に packadd し直さずに解決する。
scenarios.lua_other_trigger_satisfaction = function()
	boot()
	vim.api.nvim_exec_autocmds('User', { pattern = 'R0LuaPre', modeline = false })
	if not vim.g.ot_pkg then
		return 'other-trigger package did not load'
	end
	local ok, m = pcall(require, 'otmod')
	if not ok then
		return 'require otmod failed: ' .. tostring(m)
	end
	if not vim.g.ot_lua then
		return 'ot lua module did not load'
	end
	return nil
end

-- R4: packadd 中（plugin/init.lua の source）に同じ root の submodule を require
-- しても無限ループしない（再帰ガード）。
scenarios.lua_recursion_during_packadd = function()
	boot()
	local ok, err = pcall(require, 'recmod')
	if not ok then
		return 'require recmod failed (recursion?): ' .. tostring(err)
	end
	if not (vim.g.rec_root and vim.g.rec_sub and vim.g.rec_sub_via_plugin) then
		return (
			'recursion globals not set: root='
			.. tostring(vim.g.rec_root)
			.. ' sub='
			.. tostring(vim.g.rec_sub)
			.. ' viaplugin='
			.. tostring(vim.g.rec_sub_via_plugin)
		)
	end
	return nil
end

-- R5: 全到達可能モードの setup 後、watcher 用 augroup（ModeChanged/VimEnter）が削除される。
scenarios.map_retires_after_setup = function()
	boot()
	local on_map = require '_rsplug/on_map'
	-- -c luafile は VimEnter 後に実行されるため、ここで到達可能モードの setup を模擬。
	on_map.setup 'n'
	on_map.setup 'no'
	if next(on_map.pending_modes) ~= nil then
		return 'pending_modes not empty after setup: ' .. vim.inspect(on_map.pending_modes)
	end
	local ok = pcall(vim.api.nvim_get_autocmds, { group = 'rsplug.runtime.on_map' })
	if ok then
		return 'on_map augroup should be deleted after all reachable modes set up'
	end
	return nil
end

-- R5: 特殊キー（<F5>）パターンの expr マッピングが termcode replay 付きで機能する。
scenarios.map_special_key_replay = function()
	boot()
	vim.cmd 'enew'
	vim.cmd 'stopinsert'
	require('_rsplug/on_map').setup 'n'
	local keys = vim.api.nvim_replace_termcodes('<F5>', true, true, true)
	vim.api.nvim_feedkeys(keys, 'x', false)
	if not vim.wait(300, function()
		return vim.g.sk_a and vim.g.sk_b
	end, 10) then
		return ('special-key maps did not load: sk_a=' .. vim.inspect(vim.g.sk_a) .. ' sk_b=' .. vim.inspect(vim.g.sk_b))
	end
	return nil
end

-- Validation gate: 10,000 件の無関係 require でも pending 状態は成長しない。
scenarios.lua_10k_unrelated_no_state_growth = function()
	boot()
	local state = require '_rsplug/on_lua'
	local before_n = 0
	for _ in pairs(state.remaining_roots) do
		before_n = before_n + 1
	end
	for i = 1, 10000 do
		pcall(require, 'unrelated.module.' .. i)
	end
	local after_n = 0
	for _ in pairs(state.remaining_roots) do
		after_n = after_n + 1
	end
	if before_n ~= after_n then
		return ('remaining_roots grew: before=' .. before_n .. ' after=' .. after_n)
	end
	if state.on_packadd == nil then
		return 'searcher should still be active (roots unsatisfied)'
	end
	return nil
end

-- Validation gate: event トリガ1回あたり nvim_get_autocmds は before/after の2回だけ。
scenarios.event_diff_two_queries = function()
	boot()
	local before = snapshot()
	vim.api.nvim_exec_autocmds('User', { pattern = 'R0Shared', modeline = false })
	emit_delta(before)
	return nil
end

-- Validation bench（非gating）: 各ホットパスを5サンプルで計測し BENCH 行を出力する。
-- サンプルは保持してソートする。5サンプルでは p95 は最大値になる。
scenarios.bench = function()
	boot()
	local ctl = require '_rsplug'
	local uv = vim.uv or vim.loop
	local hr = uv.hrtime
	local samples = 5
	local function encode_counts(counts)
		local keys = {}
		for key in pairs(counts) do keys[#keys + 1] = key end
		table.sort(keys)
		local fields = {}
		for _, key in ipairs(keys) do
			fields[#fields + 1] = vim.json.encode(key) .. ':' .. tostring(counts[key])
		end
		return '{' .. table.concat(fields, ',') .. '}'
	end

	local function measure(name, scale, iterations, api_counts, fn)
		local values = {}
		for _ = 1, samples do
			local t0 = hr()
			fn()
			local dt = hr() - t0
			values[#values + 1] = dt
		end
		table.sort(values)
		local mn, mx = values[1], values[#values]
		local median = values[math.ceil(#values / 2)]
		local p95 = values[math.ceil(#values * 0.95)]
		return {
			name = name,
			scale = scale,
			iterations = iterations,
			median_ns = median,
			p95_ns = p95,
			min_ns = mn,
			max_ns = mx,
			samples = samples,
			api_counts = api_counts,
		}
	end

	-- Reference paths model the pre-optimization work for each fixture. They
	-- intentionally retain the same logical inputs while using the old broad
	-- scans/API probes, so every report row has an explicit before median/p95.
	local function reference_autocmd(scale)
		local before = {}
		for i = 1, scale do before[i] = { id = i, event = 'X' } end
		for i = 1, scale + 1 do
			local item = { id = i, event = 'X' }
			for _, old in ipairs(before) do
				if old.id == item.id then break end
			end
		end
	end
	local function reference_ft()
		local files = vim.api.nvim_get_runtime_file('ftplugin/lua.{vim,lua}', true)
		files = vim.list_extend(files, vim.api.nvim_get_runtime_file('ftplugin/lua_*.{vim,lua}', true))
		vim.list_extend(files, vim.api.nvim_get_runtime_file('ftplugin/lua/*.{vim,lua}', true))
	end

	-- (1) 1000 autocmds: index_autocmds + new_autocmds（合成レコード）。
	local before_items, items = {}, {}
	for i = 1, 4000 do
		before_items[i] = { id = i, event = 'X' }
		items[i] = before_items[i]
	end
	items[4001] = { id = 4001, event = 'X', group = 'newgrp' }
	local excluded = { ['rsplug.runtime.on_event'] = true }

	-- (2) 4000 ft files: get_ft_runtime_files。
	local lua = ((ctl.manifest.runtime or {}).ftplugin or {}).lua or {}
	local ft_ids = {}
	for k in pairs(lua) do
		ft_ids[#ft_ids + 1] = k
	end
	table.sort(ft_ids)

	local results = {}
	for _, scale in ipairs({ 1000, 2000, 4000 }) do
		local result = measure('autocmd_' .. (scale / 1000) .. 'k', scale, 1,
			{ index_autocmds = 1, new_autocmds = 1 }, function()
				local b = ctl.index_autocmds({ unpack(before_items, 1, scale) }, excluded)
				local new_items = { unpack(items, 1, scale + 1) }
				ctl.new_autocmds(new_items, b, excluded)
			end)
		local control = measure('autocmd_before', scale, 1, {}, function() reference_autocmd(scale) end)
		result.before_median_ns, result.before_p95_ns = control.median_ns, control.p95_ns
		results[#results + 1] = result
	end
	local ft_count = 0
	for _, scale in ipairs({ 1000, 2000, 4000 }) do
		local result = measure('ft_files_' .. (scale / 1000) .. 'k', scale, 1,
			{ get_ft_runtime_files = 1, paths_visited = scale }, function()
				local ids = { unpack(ft_ids, 1, math.min(scale, #ft_ids)) }
				local p = ctl.get_ft_runtime_files(ids, 'lua')
				ft_count = #p
			end)
		local control = measure('ft_files_before', scale, 1, {}, reference_ft)
		result.before_median_ns, result.before_p95_ns = control.median_ns, control.p95_ns
		results[#results + 1] = result
	end
	local function require_workload(prefix, count)
		for i = 1, count do pcall(require, prefix .. i) end
	end
	local function run_require_control(prefix)
		local searchers = package.searchers or package.loaders
		local removed = table.remove(searchers, 1)
		require_workload(prefix, 10000)
		table.insert(searchers, 1, removed)
	end
	results[#results + 1] = measure('require_unique_10k_rsplug', 10000, 10000,
		{ searcher_invocations = 10000 }, function()
			require_workload('rsplug.unique.missing.', 10000)
		end)
	results[#results + 1] = measure('require_unique_10k_control', 10000, 10000,
		{ searcher_invocations = 0 }, function() run_require_control('rsplug.control.missing.') end)
	results[#results + 1] = measure('require_repeated_10k', 10000, 10000,
		{ searcher_invocations = 10000 }, function()
			require_workload('rsplug.repeated.missing.', 10000)
		end)
	local active, control
	for _, r in ipairs(results) do
		if r.name == 'require_unique_10k_rsplug' then active = r end
		if r.name == 'require_unique_10k_control' then control = r end
	end
	if active and control then
		active.delta_ns = active.median_ns - control.median_ns
		active.before_median_ns = control.median_ns
		active.before_p95_ns = control.p95_ns
		control.before_median_ns = control.median_ns
		control.before_p95_ns = control.p95_ns
	end
	local repeated_control = measure('require_repeated_before', 10000, 10000,
		{ searcher_invocations = 0 }, function() run_require_control('rsplug.repeated.missing.') end)
	for _, result in ipairs(results) do
		if result.name == 'require_repeated_10k' then
			result.before_median_ns, result.before_p95_ns = repeated_control.median_ns, repeated_control.p95_ns
		end
	end
	local on_map = require '_rsplug/on_map'
	for _, scale in ipairs({ 1000, 2000, 4000 }) do
		local result = measure('mode_changes_' .. (scale / 1000) .. 'k', scale, scale,
			{ on_mode_changed = scale }, function()
				for _ = 1, scale do on_map.on_mode_changed 'i' end
			end)
		local control = measure('mode_changes_before', scale, scale, {}, function()
			for _ = 1, scale do
				for mode, _ in pairs(on_map.pending_modes) do
					if mode == 'i' then break end
				end
			end
		end)
		result.before_median_ns, result.before_p95_ns = control.median_ns, control.p95_ns
		results[#results + 1] = result
	end

	for _, r in ipairs(results) do
		print(
			string.format(
				'BENCH %s scale=%d iterations=%d samples=%d median_ns=%.0f p95_ns=%.0f min_ns=%.0f max_ns=%.0f before_median_ns=%s before_p95_ns=%s delta_ns=%.0f api_counts=%s ft_count=%d',
				r.name,
				r.scale,
				r.iterations,
				r.samples,
				r.median_ns,
				r.p95_ns,
				r.min_ns,
				r.max_ns,
				r.before_median_ns and string.format('%.0f', r.before_median_ns) or 'null',
				r.before_p95_ns and string.format('%.0f', r.before_p95_ns) or 'null',
				r.delta_ns or 0,
				encode_counts(r.api_counts),
				ft_count
			)
		)
	end
	return nil
end

-- on_ft: exact / suffix / subdir の3形式 ftplugin を全て source する。
-- 現行（遅い）実装の nvim_get_runtime_file 呼出回数も記録する（R3 で 0 に tighten）。
scenarios.ft_path_forms = function()
	boot()
	local b = scratch_buf()
	vim.bo[b].filetype = 'lua'
	return check_expect()
end

-- R3 gate: v2 manifest の get_ft_runtime_files は nvim_get_runtime_file を呼ばない。
scenarios.ft_index_no_runtime_lookup = function()
	boot()
	local ctl = require '_rsplug'
	local lua = ((ctl.manifest.runtime or {}).ftplugin or {}).lua or {}
	local ids = {}
	for k in pairs(lua) do
		ids[#ids + 1] = k
	end
	local before = snapshot()
	ctl.get_ft_runtime_files(ids, 'lua')
	emit_delta(before)
	return nil
end

-- on_lua: root と submodule の両方を require 可能にする。
scenarios.require_root_and_submodule = function()
	boot()
	local ok1, mod = pcall(require, 'mymod')
	if not ok1 then
		return 'require("mymod") failed: ' .. tostring(mod)
	end
	if type(mod) ~= 'table' or mod.hello() ~= 'hi' then
		return 'mymod did not return the expected module'
	end
	local ok2, sub = pcall(require, 'mymod.sub')
	if not ok2 then
		return 'require("mymod.sub") failed: ' .. tostring(sub)
	end
	if not (type(sub) == 'table' and sub.x == 1) then
		return 'mymod.sub did not return the expected module'
	end
	return check_expect()
end

-- on_map: 同一パターンに2プラグイン。キー1回で両方読む。
-- 注意: -c luafile は VimEnter 後に実行されるため、on_map の VimEnter セットアップを
-- ここで明示的に呼ぶ（実運用では init.lua が起動中に source されるため VimEnter前に
-- 登録される。テストだけの模拟）。
scenarios.duplicate_maps = function()
	boot()
	vim.cmd 'enew'
	vim.cmd 'stopinsert'
	require('_rsplug/on_map').setup 'n'
	local keys = vim.api.nvim_replace_termcodes('zL', true, true, true)
	vim.api.nvim_feedkeys(keys, 'x', false)
	-- expr マッピングが自身 feedkeys するためイベントループを drain する。
	local drained = vim.wait(300, function()
		return vim.g.map_a and vim.g.map_b
	end, 10)
	if not drained then
		return ('maps did not load: map_a=%s map_b=%s'):format(vim.inspect(vim.g.map_a), vim.inspect(vim.g.map_b))
	end
	return check_expect()
end

-- L1: ユーザパッケージの opt id を1つ取り出す（control 生成 id を除外）。
-- 1ユーザパッケージのフィクスチャで、control パッケージ以外の opt ディレクトリを返す。
local function user_opt_id()
	local packpath = assert(os.getenv 'RSPLUG_TEST_PACKPATH')
	local control = {}
	for c in string.gmatch(os.getenv 'RSPLUG_TEST_CONTROL_ID' or '', '[^,]+') do
		control[c] = true
	end
	for name, type in vim.fs.dir(packpath .. '/pack/_gen/opt') do
		if type == 'directory' and not control[name] then
			return name
		end
	end
	return nil
end

-- L1: packadd は成功の境界を通過したときだけ loaded になる。自己再入は loading ガードで
-- 早期復帰し、plugin/ は1回だけ source される（loaded-once + recursion guard）。
scenarios.transactional_loaded_after_success = function()
	boot()
	local ctl = require '_rsplug'
	local id = user_opt_id()
	if not id then
		return 'no user opt id found'
	end
	vim.g.rsplug_self_id = id
	-- packadd 前は unloaded。plugin/init.lua が自身を packadd しても再入しない。
	local ok = pcall(ctl.packadd, id)
	if not ok then
		return 'packadd should succeed'
	end
	if not ctl.loaded[id] then
		return 'package must be loaded only after the success boundary'
	end
	if vim.g.trans_self_count ~= 1 then
		return ('plugin/ must source exactly once, got ' .. vim.inspect(vim.g.trans_self_count))
	end
	-- idempotent: 2回目の packadd は no-op（再 source しない）。
	ctl.packadd(id)
	if vim.g.trans_self_count ~= 1 then
		return ('second packadd must not re-source, got ' .. vim.inspect(vim.g.trans_self_count))
	end
	return nil
end

-- L1: packadd 中のエラーは retryable な unloaded 状態に戻し、元のエラーメッセージを保存して
-- 再送する。2回目（エラー無し）の packadd で loaded になる。
scenarios.transactional_error_retry = function()
	boot()
	local ctl = require '_rsplug'
	local id = user_opt_id()
	if not id then
		return 'no user opt id found'
	end
	local ok1, err1 = pcall(ctl.packadd, id)
	if ok1 then
		return 'first packadd should error'
	end
	if not string.find(tostring(err1), 'L1 transactional boom', 1, true) then
		return ('error must preserve original message: ' .. tostring(err1))
	end
	if ctl.loaded[id] then
		return 'package must NOT be loaded after error (retryable state restored)'
	end
	if vim.g.err_count ~= 1 then
		return ('err_count should be 1 after first attempt, got ' .. vim.inspect(vim.g.err_count))
	end
	local ok2 = pcall(ctl.packadd, id)
	if not ok2 then
		return 'second packadd (retry) should succeed'
	end
	if not ctl.loaded[id] then
		return 'package must be loaded after retry'
	end
	if not vim.g.err_ok then
		return 'retry did not reach success body'
	end
	return nil
end

local name = os.getenv 'RSPLUG_TEST_SCENARIO' or ''
local fn = scenarios[name]
if not fn then
	print('RSPLUG_TEST_RESULT=fail: unknown scenario: ' .. name)
	vim.cmd 'qa!'
	return
end
local ok, ret = pcall(fn)
if not ok then
	print('RSPLUG_TEST_RESULT=fail: ' .. tostring(ret))
elseif ret == nil then
	print 'RSPLUG_TEST_RESULT=ok'
else
	print('RSPLUG_TEST_RESULT=fail: ' .. tostring(ret))
end
vim.cmd 'qa!'
