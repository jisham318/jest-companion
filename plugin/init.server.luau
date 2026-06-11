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
	-- How many seconds to let the run go before abandoning it (mirrors the server's
	-- --run-timeout). Absent on older servers; we fall back to a default.
	runTimeout: number?,
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

-- The one-shot CLI listens on a fixed port; each long-running MCP server (one per agent) binds the
-- first free port in a range, so several can run at once. We poll the CLI every cycle (a human may
-- be waiting on it) plus ONE MCP port per cycle, round-robin, and serve whichever has a run
-- pending. Polling one MCP port at a time keeps us well under Studio's HTTP request budget no
-- matter how many MCP servers are running; runs still execute one at a time since Studio is serial.
local CLI_URL = "http://127.0.0.1:28860"
local MCP_PORT_MIN = 28861
local MCP_PORT_MAX = 28868
local mcpCursor = MCP_PORT_MIN
-- Stashed on Workspace so the patched FileSystemService/Writeable modules (which get cloned into
-- the Jest tree, away from this script) know which server to report a run's output back to.
local SERVER_URL_ATTRIBUTE = "__JestCompanion_ServerUrl"
local PROTOCOL_VERSION = "3"

local function nextMcpUrl(): string
	local url = `http://127.0.0.1:{mcpCursor}`
	mcpCursor += 1
	if mcpCursor > MCP_PORT_MAX then
		mcpCursor = MCP_PORT_MIN
	end
	return url
end

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

-- Used when the server didn't send a runTimeout (an older server). Matches the server's
-- --run-timeout default.
local DEFAULT_RUN_TIMEOUT_SECONDS = 300

function runForServer(url: string, body: TestBody)
	-- Tell the patched filesystem/write modules which server to report this run's output back to.
	Workspace:SetAttribute(SERVER_URL_ATTRIBUTE, url)

	-- Run the suite on its own thread and watch it from here. A suite that never finishes (an
	-- unresolved promise, a stuck async test) used to block this loop forever, which killed test
	-- running for EVERY server until the place was reopened — so after the run timeout we abandon
	-- the run, tell its server, and get back to polling.
	local finished = false
	local runThread = task.spawn(function()
		local success, outputOrErr = pcall(runTests, body)
		if success then
			local sendSuccess, sendErr = pcall(sendResults, url, outputOrErr)
			if not sendSuccess then
				warn(`Failed to send output: {sendErr}`)
			end
		else
			warn(`Test run error: {outputOrErr}`)

			local reportSuccess, reportErr = pcall(reportError, url)
			if not reportSuccess then
				warn(`Failed to report error: {reportErr}`)
			end
		end
		finished = true
	end)

	local timeout = body.runTimeout or DEFAULT_RUN_TIMEOUT_SECONDS
	local deadline = os.clock() + timeout
	while not finished and os.clock() < deadline do
		task.wait(0.5)
	end

	if not finished then
		-- Cancellation only lands if the run is parked at a yield point; a busy-loop freezes
		-- Studio outright and nothing can save that.
		pcall(task.cancel, runThread)
		warn(`Test run didn't finish within {timeout}s; abandoning it so other runs can proceed.`)
		local reportSuccess, reportErr = pcall(reportError, url)
		if not reportSuccess then
			warn(`Failed to report error: {reportErr}`)
		end
	end
end

function loop()
	-- Poll the CLI (time-sensitive, a human may be waiting) plus one MCP port this cycle, and serve
	-- the first that has a run pending. Idle servers answer run == false, so we just move on.
	local urls = { CLI_URL, nextMcpUrl() }
	for _, url in urls do
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
