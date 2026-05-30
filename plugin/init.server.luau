local RunService = game:GetService("RunService")
local Workspace = game:GetService("Workspace")
local ReplicatedStorage = game:GetService("ReplicatedStorage")
local HttpService = game:GetService("HttpService")
local LogService = game:GetService("LogService")

local hotReload = require(script.hotReload)
local pathToInstance = require(script.pathToInstance)

function getJest(): any
	local Jest = ReplicatedStorage:FindFirstChild("Jest", true)
	assert(Jest, "Couldn't find Jest!")
	assert(Jest:IsA("ModuleScript"), "Jest is not a ModuleScript!")

	patchJest(Jest)
	hotReload.flush()

	local mod = hotReload.require(Jest)
	return mod
end

function patchJest(Jest: ModuleScript)
	local Packages = Jest.Parent
	assert(Packages, "Jest has no parent")

	local RobloxShared = Packages:FindFirstChild("RobloxShared")
	assert(RobloxShared, "Jest has no RobloxShared next to it!")

	local originalGetDataModelService = RobloxShared:FindFirstChild("getDataModelService")
	assert(originalGetDataModelService, "RobloxShared has no getDataModelService module under it")

	originalGetDataModelService:Destroy()

	local getDataModelService = script.patch.getDataModelService:Clone()
	getDataModelService.Parent = RobloxShared

	local originalFileSystemService = RobloxShared:FindFirstChild("FileSystemService")
	if originalFileSystemService then
		originalFileSystemService:Destroy()
	end

	local FileSystemService = script.patch.FileSystemService:Clone()
	FileSystemService.Parent = RobloxShared

	local originalCoreScriptSyncService = RobloxShared:FindFirstChild("CoreScriptSyncService")
	if originalCoreScriptSyncService then
		originalCoreScriptSyncService:Destroy()
	end

	local CoreScriptSyncService = script.patch.CoreScriptSyncService:Clone()
	CoreScriptSyncService.Parent = RobloxShared

	local originalWritable = RobloxShared:FindFirstChild("Writeable")
	if originalWritable then
		originalWritable:Destroy()
	end

	local Writeable = script.patch.Writeable:Clone()
	Writeable.Parent = RobloxShared

	local originalChalk = Packages:FindFirstChild("ChalkLua")
	if originalChalk then
		originalChalk:Destroy()
	end

	local Chalk = script.patch.ChalkLua:Clone()
	Chalk.Parent = Packages
end

type TestBody = {
	projects: { string },
	options: {
		[string]: unknown?,
	},
	-- When false, the server has nothing for us to run right now (used by the MCP server, which
	-- keeps the server alive between runs). Absent or true means run as normal.
	run: boolean?,
}

type TestResults = {
	success: boolean,
}

function runTests(body: TestBody): TestResults
	LogService:ClearOutput()

	local projects: { Instance } = {}

	for _, project in body.projects do
		local instance = pathToInstance(project)
		assert(instance, `Couldn't find instance for '{project}'`)

		table.insert(projects, instance)
	end

	body.options.setupFiles = { script.setup }

	local Jest = getJest()
	local result = Jest.runCLI(script, body.options, projects):expect()
	return {
		success = result.results.numFailedTestSuites == 0 and result.results.numFailedTests == 0,
	}
end

-- The CLI and the long-running MCP server listen on different ports so both can run at the same
-- time. We poll each of them and serve whichever one has a run pending.
local CLI_URL = "http://127.0.0.1:28860"
local MCP_URL = "http://127.0.0.1:28861"
local SERVER_URLS = { CLI_URL, MCP_URL }
-- Stashed on Workspace so the patched FileSystemService/Writeable modules (which get cloned into
-- the Jest tree, away from this script) know which server to report a run's output back to.
local SERVER_URL_ATTRIBUTE = "__JestCompanion_ServerUrl"
local PROTOCOL_VERSION = "3"

function poll(url: string): TestBody?
	local reqSuccess, response = pcall(function()
		return HttpService:RequestAsync({
			Url = `{url}/poll`,
			Method = "POST",
			Body = HttpService:JSONEncode({
				protocolVersion = PROTOCOL_VERSION,
				rojoConnected = Workspace:GetAttribute("__Rojo_ConnectionUrl") ~= nil,
			}),
			Compress = Enum.HttpCompression.None,
			Headers = { ["Content-Type"] = "application/json" },
		})
	end)

	if not reqSuccess then
		-- They aren't running the CLI
		return nil
	end

	if not response.Success then
		error(`Server rejected poll: ({response.StatusCode})\n{response.Body}`)
	end

	if not response.Body then
		error("Server didn't send back anything")
	end

	return HttpService:JSONDecode(response.Body) :: TestBody
end

function sendResults(url: string, results: TestResults)
	local reqSuccess, response = pcall(function()
		return HttpService:RequestAsync({
			Url = `{url}/results`,
			Method = "POST",
			Body = HttpService:JSONEncode(results),
			Headers = { ["Content-Type"] = "application/json" },
			Compress = Enum.HttpCompression.None,
		})
	end)

	if not reqSuccess then
		error("Failed to send output to server because it wasn't running")
	end

	if not response.Success then
		error(`Server rejected output: {response.StatusCode}\n{response.Body}`)
	end
end

function reportError(url: string)
	local reqSuccess = pcall(function()
		return HttpService:RequestAsync({
			Url = `{url}/run-error`,
			Method = "POST",
			Compress = Enum.HttpCompression.None,
		})
	end)

	if not reqSuccess then
		error("Failed to let server know there was an error")
	end

	return nil
end

function runForServer(url: string, body: TestBody)
	-- Tell the patched filesystem/write modules which server to report this run's output back to.
	Workspace:SetAttribute(SERVER_URL_ATTRIBUTE, url)

	local output: TestResults
	do
		local success, outputOrErr = pcall(runTests, body)
		if not success then
			warn(`Test run error: {outputOrErr}`)

			local reportSuccess, reportErr = pcall(reportError, url)
			if not reportSuccess then
				warn(`Failed to report error: {reportErr}`)
			end

			return
		end

		output = outputOrErr
	end

	local success, err = pcall(sendResults, url, output)
	if not success then
		warn(`Failed to send output: {err}`)
	end
end

function loop()
	-- Poll each server in turn and serve the first one that has a run pending. The CLI is one-shot
	-- and time-sensitive, so it goes first; the MCP server stays idle (run == false) between runs.
	for _, url in SERVER_URLS do
		local success, bodyOrErr = pcall(poll, url)
		if not success then
			warn(`Failed to poll server at {url}: {bodyOrErr}`)
			continue
		end

		local body = bodyOrErr :: TestBody?
		if body == nil then
			-- That server isn't running; try the next one.
			continue
		end

		-- The server (currently only in MCP mode) can tell us to stay idle when there's no run
		-- pending, so we don't run the whole suite on every poll.
		if body.run == false then
			continue
		end

		runForServer(url, body)
		return
	end
end

if not RunService:IsRunning() then
	local running = true

	-- A reinstall or hot-reload (e.g. drillbit overwriting the .rbxm while Studio is open) starts a
	-- fresh copy of this script but doesn't reliably stop the old one's loop. Stop ours when the
	-- plugin unloads so we don't leave stale loops polling the server every 0.5s forever.
	local pluginRef: Plugin? = plugin
	if pluginRef then
		pluginRef.Unloading:Connect(function()
			running = false
		end)
	end

	while running do
		loop()
		task.wait(0.5)
	end
end
