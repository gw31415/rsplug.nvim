return {
	---@param pkg string
	packadd = function(pkg)
		local setup_scripts = vim.g._rsplug_setup_scripts[pkg] or {};
		vim.cmd.packadd(pkg)
		if setup_scripts.lua_source then
			require(setup_scripts.lua_source)
		end
	end,
}
