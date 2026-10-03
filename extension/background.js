// Linet Handoff: отправить страницу на телефон через native messaging host linet-nmh.
const api = globalThis.browser ?? globalThis.chrome;
const HOST = "org.linet";

async function send(tab, url, title) {
  if (!url || !/^https?:\/\//.test(url)) {
    return flash(tab, "!", "Можно отправить только http(s)-страницу");
  }
  try {
    const reply = await api.runtime.sendNativeMessage(HOST, { type: "open", url, title: title ?? "" });
    if (reply?.ok) {
      flash(tab, "✓", "Открыто на телефоне");
    } else {
      flash(tab, "!", reply?.error ?? "Ошибка Linet");
    }
  } catch (e) {
    flash(tab, "!", `linet-nmh недоступен: ${e.message ?? e}`);
  }
}

// Короткая отметка на кнопке вместо уведомлений: не нужно лишнее разрешение.
function flash(tab, text, title) {
  const tabId = tab?.id;
  api.action.setBadgeText({ text, tabId });
  api.action.setTitle({ title, tabId });
  setTimeout(() => {
    api.action.setBadgeText({ text: "", tabId });
    api.action.setTitle({ title: "Открыть на телефоне", tabId });
  }, 3000);
}

api.action.onClicked.addListener((tab) => send(tab, tab.url, tab.title));

api.commands.onCommand.addListener(async (command, tab) => {
  if (command !== "send-tab") return;
  tab ??= (await api.tabs.query({ active: true, currentWindow: true }))[0];
  send(tab, tab?.url, tab?.title);
});

api.runtime.onInstalled.addListener(() => {
  api.contextMenus.create({ id: "link", title: "Открыть ссылку на телефоне", contexts: ["link"] });
  api.contextMenus.create({ id: "page", title: "Открыть страницу на телефоне", contexts: ["page"] });
});

api.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId === "link") send(tab, info.linkUrl, info.linkText ?? "");
  else send(tab, info.pageUrl, tab?.title);
});
