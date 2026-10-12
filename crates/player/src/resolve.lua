local mp = require "mp"
local msg = require "mp.msg"

local pending = {}
local fallback = {}
local next_request = 0

local function finish(request, original, target)
    local held = pending[request]
    if held == nil or held.original ~= original then
        return
    end
    pending[request] = nil
    held.timer:kill()
    if mp.get_property("path", "") == original and target ~= "" then
        mp.set_property("stream-open-filename", target)
    end
    held.hook:cont()
end

local function fail(request, original)
    local held = pending[request]
    if held == nil or held.original ~= original then
        return
    end
    if mp.get_property("path", "") == original then
        mp.commandv("stop")
    end
    finish(request, original, "")
end

mp.register_script_message("redwax-resolved", finish)
mp.register_script_message("redwax-resolution-failed", fail)
mp.register_script_message("redwax-fallback", function(request, path)
    local marker = request
    fallback[path] = fallback[path] or {}
    table.insert(fallback[path], marker)
    mp.add_timeout(5, function()
        local markers = fallback[path] or {}
        for index, candidate in ipairs(markers) do
            if candidate == marker then
                table.remove(markers, index)
                break
            end
        end
        if #markers == 0 then
            fallback[path] = nil
        end
    end)
    mp.commandv("script-message", "redwax-fallback-ready", request, path)
end)

mp.register_script_message("redwax-fallback-cancel", function(request, path)
    local markers = fallback[path] or {}
    for index, marker in ipairs(markers) do
        if marker == request then
            table.remove(markers, index)
            break
        end
    end
    if #markers == 0 then
        fallback[path] = nil
    end
    mp.commandv("script-message", "redwax-fallback-cancelled", request, path)
end)

mp.add_hook("on_load", 5, function(hook)
    local original = mp.get_property("path", "")
    if not original:match("^https?://") then
        return
    end
    local markers = fallback[original]
    if markers ~= nil and #markers > 0 then
        table.remove(markers, 1)
        if #markers == 0 then
            fallback[original] = nil
        end
        return
    end

    next_request = next_request + 1
    local request = tostring(next_request)
    hook:defer()
    local timer = mp.add_timeout(50, function()
        if pending[request] ~= nil then
            msg.error("Audio resolution timed out")
            fail(request, original)
        end
    end)
    pending[request] = {
        hook = hook,
        original = original,
        timer = timer,
    }
    mp.commandv("script-message", "redwax-resolve", request, original)
end)
