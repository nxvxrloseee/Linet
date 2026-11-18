-- Only windows started by Linet; SDL may set its class after window.open.
local function linet_scrcpy(win)
    if not (win and win.class == "scrcpy" and win.title == "Телефон") or win.floating then
        return
    end
    hl.dispatch(hl.dsp.window.float({ action = "on", window = win }))
    hl.dispatch(hl.dsp.window.center({ window = win }))
end
hl.on("window.open", linet_scrcpy)
hl.on("window.title", linet_scrcpy)
