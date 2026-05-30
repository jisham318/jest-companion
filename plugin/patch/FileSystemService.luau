-- jest-companion inserted this file

local HttpService = game:GetService("HttpService")
local Workspace = game:GetService("Workspace")

local FileSystemService = {}

-- jest-companion's runner (init.server.luau) stashes the URL of the server that dispatched the
-- current run here, so filesystem ops go back to the right one (the CLI and MCP server use
-- different ports). Fall back to the CLI port for safety.
local function fsUrl(): string
	return (Workspace:GetAttribute("__JestCompanion_ServerUrl") or "http://127.0.0.1:28860") .. "/fs"
end

local function encodeSegments(p)
	local parts = string.split(p, "/")
	for i, seg in parts do
		parts[i] = HttpService:UrlEncode(seg)
	end
	return table.concat(parts, "/")
end

local function request(op, method, path, body)
	local encoded = encodeSegments(path)
	local URL = fsUrl()
	local url
	if op == "file" then
		url = `{URL}/file/{encoded}`
	elseif op == "dir" then
		url = `{URL}/dir/{encoded}`
	elseif op == "exists" then
		url = `{URL}/exists/{encoded}`
	else
		error(`unknown op {op}`)
	end

	local response = HttpService:RequestAsync({
		Url = url,
		Method = method,
		Body = body,
		Headers = { ["Content-Type"] = "text/plain" },
		Compress = Enum.HttpCompression.None,
	})

	if not response.Success then
		error(`Server rejected filesystem op: {response.StatusCode}\n{response.Body}`)
	end

	return response
end

function FileSystemService:WriteFile(path, contents)
	request("file", "PUT", path, contents)
end

function FileSystemService:CreateDirectories(path)
	request("dir", "PUT", path)
end

function FileSystemService:Exists(path)
	local resp = request("exists", "GET", path)
	return resp.Success
end

function FileSystemService:Remove(path)
	request("file", "DELETE", path)
end

function FileSystemService:IsRegularFile(path)
	return self:Exists(path)
end

return FileSystemService
