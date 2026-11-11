// Autokan front-end glue: drag and drop, the card modal, media uploads, the agent console and
// the "Send To Agent" dock.
// Everything else is server-rendered HTML driven by htmx.
(() => {
  const $ = (sel, root = document) => root.querySelector(sel);
  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  };

  // ---- small utilities -------------------------------------------------------------------

  function toast(message, kind = "error") {
    const t = el("div", `alert alert-${kind} text-sm shadow-lg max-w-sm`, message);
    $("#toasts").append(t);
    setTimeout(() => t.remove(), 6000);
  }

  function localTimes(root) {
    root.querySelectorAll("time[data-local]").forEach((t) => {
      const d = new Date(t.dateTime);
      if (!isNaN(d)) t.textContent = d.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
    });
  }
  localTimes(document);

  async function post(url, data) {
    const r = await fetch(url, { method: "POST", body: data ? new URLSearchParams(data) : undefined });
    if (!r.ok) throw new Error((await r.text()) || `Request failed (${r.status})`);
    return r;
  }

  const refreshBoard = () => htmx.trigger(document.body, "board-refresh");

  // ---- themes ----------------------------------------------------------------------------

  // An empty choice means "match the system light / dark setting".
  const systemTheme = () => (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  function markTheme() {
    const current = localStorage.getItem("theme") || "";
    document.querySelectorAll("[data-set-theme]").forEach((b) => {
      const on = b.dataset.setTheme === current;
      b.querySelector(".theme-check").classList.toggle("invisible", !on);
      b.classList.toggle("bg-base-200", on);
    });
  }
  function setTheme(theme) {
    if (theme) localStorage.setItem("theme", theme);
    else localStorage.removeItem("theme");
    document.documentElement.dataset.theme = theme || systemTheme();
    markTheme();
  }
  markTheme();
  matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
    if (!localStorage.getItem("theme")) setTheme("");
  });

  document.addEventListener("click", (e) => {
    const themeOption = e.target.closest("[data-set-theme]");
    if (themeOption) setTheme(themeOption.dataset.setTheme);
    const toggle = e.target.closest("[data-toggle]");
    if (toggle) {
      const scope = toggle.closest("[data-toggle-scope]") || document;
      scope.querySelectorAll(toggle.dataset.toggle).forEach((n) => n.classList.toggle("hidden"));
    }
  });

  document.addEventListener("submit", (e) => {
    const msg = e.target.dataset.confirm;
    if (msg && !confirm(msg)) e.preventDefault();
  });

  document.body.addEventListener("htmx:responseError", (e) => toast(e.detail.xhr.responseText || "Request failed"));
  document.body.addEventListener("htmx:sendError", () => toast("Could not reach the server"));

  // ---- card / column modal ---------------------------------------------------------------

  const modal = $("#modal");
  if (modal) {
    document.body.addEventListener("close-modal", () => modal.close());
    document.addEventListener("click", (e) => {
      if (e.target.closest("[data-close-modal]")) modal.close();
    });
    modal.addEventListener("close", () => ($("#modal-body").innerHTML = ""));
  }

  // A link such as /boards/3?card=17 (used in Jira issues) opens that card straight away.
  const linkedCard = new URLSearchParams(location.search).get("card");
  if (modal && /^\d+$/.test(linkedCard || "")) {
    htmx.ajax("GET", `/cards/${linkedCard}`, { target: "#modal-body", swap: "innerHTML" });
    history.replaceState(null, "", location.pathname);
  }

  // ---- board drag and drop ---------------------------------------------------------------

  let refocusColumn = null;

  function initBoard() {
    const board = $("#board");
    if (!board || !window.Sortable) return;
    board.querySelectorAll(".card-list").forEach((list) => {
      new Sortable(list, {
        group: "cards",
        animation: 150,
        draggable: ".card-item",
        filter: ".locked", // cards this user may not move
        preventOnFilter: false,
        onEnd: (ev) => {
          if (ev.from === ev.to && ev.oldDraggableIndex === ev.newDraggableIndex) return;
          post(`/cards/${ev.item.dataset.cardId}/move`, { column_id: ev.to.dataset.columnId, position: ev.newDraggableIndex })
            .catch((err) => toast(err.message))
            .finally(refreshBoard);
        },
      });
    });
    const columns = $("#columns");
    if (columns && board.dataset.manage === "true") {
      new Sortable(columns, {
        animation: 150,
        draggable: ".column",
        handle: ".column-handle",
        filter: "button",
        preventOnFilter: false,
        onEnd: (ev) => {
          if (ev.oldDraggableIndex === ev.newDraggableIndex) return;
          const ids = [...columns.querySelectorAll(".column")].map((c) => c.dataset.columnId).join(",");
          post(`/boards/${board.dataset.boardId}/columns/order`, { ids })
            .catch((err) => toast(err.message))
            .finally(refreshBoard);
        },
      });
    }
    if (refocusColumn) {
      $(`.column[data-column-id="${refocusColumn}"] input[name=title]`)?.focus();
      refocusColumn = null;
    }
  }
  initBoard();

  // Drag any empty part of the board with the left mouse button to scroll it sideways.
  const boardEl = $("#board");
  if (boardEl) {
    let pan = null;
    boardEl.addEventListener("mousedown", (e) => {
      if (e.button !== 0 || e.target.closest(".column, form, button, input, textarea, a")) return;
      pan = { x: e.clientX, left: boardEl.scrollLeft };
      boardEl.classList.add("panning");
      e.preventDefault(); // no text selection while panning
    });
    window.addEventListener("mousemove", (e) => {
      if (pan) boardEl.scrollLeft = pan.left - (e.clientX - pan.x);
    });
    window.addEventListener("mouseup", () => {
      pan = null;
      boardEl.classList.remove("panning");
    });
  }

  // Keep the "+ Add a card" box focused so several cards can be typed in a row.
  document.body.addEventListener("htmx:afterRequest", (e) => {
    const col = e.detail.elt.closest?.(".column");
    if (e.detail.successful && col && e.detail.elt.matches("form")) refocusColumn = col.dataset.columnId;
  });

  document.body.addEventListener("htmx:afterSwap", (e) => {
    const id = e.detail.target.id;
    if (id === "board") initBoard();
    localTimes(e.detail.target); // any swapped fragment may carry timestamps
    if (id === "modal-body") {
      if (modal && !modal.open && e.detail.target.innerHTML.trim()) modal.showModal();
    }
    if (id === "agent-messages") scrollAgent();
  });

  // ---- GitHub / GitLab sync ---------------------------------------------------------------

  // The link form shows only the fields of the chosen service.
  document.addEventListener("change", (e) => {
    if (!e.target.matches("[data-show-for]")) return;
    e.target
      .closest("form")
      .querySelectorAll("[data-provider]")
      .forEach((n) => n.classList.toggle("hidden", n.dataset.provider !== e.target.value));
  });

  // A linked board changes without anyone touching it here, so re-read it now and then, but never
  // while the person is in the middle of something.
  if (boardEl?.dataset.synced) {
    setInterval(() => {
      const typing = ["INPUT", "TEXTAREA"].includes(document.activeElement?.tagName);
      const busy = document.querySelector("#modal[open], .sortable-chosen") || boardEl.classList.contains("panning");
      if (!typing && !busy && !document.hidden) refreshBoard();
    }, 20000);
  }

  // ---- image / video uploads in Markdown editors -----------------------------------------

  function insertAtCursor(ta, text) {
    const start = ta.selectionStart ?? ta.value.length;
    ta.value = ta.value.slice(0, start) + text + ta.value.slice(ta.selectionEnd ?? start);
    ta.selectionStart = ta.selectionEnd = start + text.length;
    ta.focus();
  }

  async function uploadFiles(editor, files) {
    const ta = $("textarea", editor);
    const status = $("[data-upload-status]", editor);
    for (const file of files) {
      status.textContent = `Uploading ${file.name}…`;
      const body = new FormData();
      body.append("file", file, file.name);
      try {
        const r = await fetch(editor.dataset.uploadUrl, { method: "POST", body });
        if (!r.ok) throw new Error((await r.text()) || "Upload failed");
        insertAtCursor(ta, `\n${(await r.json()).markdown}\n`);
        status.textContent = "Uploaded. Save to publish.";
      } catch (err) {
        status.textContent = "Upload failed.";
        toast(err.message);
      }
    }
  }

  document.addEventListener("change", (e) => {
    if (!e.target.matches(".editor input[type=file]")) return;
    uploadFiles(e.target.closest(".editor"), [...e.target.files]);
    e.target.value = "";
  });
  document.addEventListener("paste", (e) => {
    const files = [...(e.clipboardData?.files || [])];
    if (!e.target.matches?.(".editor textarea") || !files.length) return;
    e.preventDefault();
    uploadFiles(e.target.closest(".editor"), files);
  });
  document.addEventListener("dragover", (e) => {
    if (e.target.matches?.(".editor textarea")) e.preventDefault();
  });
  document.addEventListener("drop", (e) => {
    const files = [...(e.dataTransfer?.files || [])];
    if (!e.target.matches?.(".editor textarea") || !files.length) return;
    e.preventDefault();
    uploadFiles(e.target.closest(".editor"), files);
  });

  // ---- agent console ---------------------------------------------------------------------

  const panel = $("#agent-panel");
  const box = $("#agent-messages");
  function scrollAgent() {
    if (box) box.scrollTop = box.scrollHeight;
  }

  // POST one agent turn and pass each event of the server's stream to `handle`.
  async function streamTurn(url, data, handle, signal) {
    const r = await fetch(url, { method: "POST", body: data ? new URLSearchParams(data) : undefined, signal });
    if (!r.ok) throw new Error((await r.text()) || `Request failed (${r.status})`);
    const reader = r.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      let i;
      while ((i = buf.indexOf("\n\n")) >= 0) {
        const block = buf.slice(0, i);
        buf = buf.slice(i + 2);
        for (const line of block.split("\n")) {
          if (line.startsWith("data:")) handle(JSON.parse(line.slice(5)));
        }
      }
    }
  }

  if (panel) {
    const boardId = panel.dataset.boardId;
    const form = $("#agent-form");
    const input = $("textarea", form);
    const sendBtn = $("button", form);
    let busy = false;

    const setOpen = (open) => {
      panel.classList.toggle("hidden", !open);
      panel.classList.toggle("flex", open);
      localStorage.setItem("agentOpen", open ? "1" : "0");
      if (open) scrollAgent();
    };
    if (localStorage.getItem("agentOpen") === "1") setOpen(true);

    const bubble = (side, cls, text) => {
      const wrap = el("div", `chat chat-${side}`);
      const b = el("div", `chat-bubble text-sm whitespace-pre-wrap break-words ${cls}`, text);
      wrap.append(b);
      box.append(wrap);
      return b;
    };
    const errorLine = (message) => box.append(el("div", "alert alert-error text-sm my-1", message));

    // Send one console turn and render the server's event stream (text deltas, tool calls, board refreshes).
    async function run(url, data, userText) {
      if (busy) return;
      busy = true;
      sendBtn.disabled = true;
      setOpen(true);
      if (!box.querySelector(".chat")) box.innerHTML = "";
      bubble("end", "chat-bubble-primary", userText);
      const status = el("div", "flex items-center gap-2 text-xs opacity-60 pl-2 py-1");
      status.append(el("span", "loading loading-dots loading-xs"), el("span", "", "Working…"));
      box.append(status);
      scrollAgent();

      let live = null;
      let error = null;
      const handle = (ev) => {
        if (ev.type === "delta") {
          if (!live) {
            live = bubble("start", "bg-base-200 text-base-content", "");
            box.append(status);
          }
          live.textContent += ev.text;
        } else if (ev.type === "tool") {
          live = null;
          status.before(el("div", `text-xs font-mono pl-2 break-words ${ev.ok ? "opacity-70" : "text-error"}`, `🔧 ${ev.name} ${ev.arguments}`));
        } else if (ev.type === "meta") {
          status.lastChild.textContent = `Working… · ${ev.model}`;
        } else if (ev.type === "refresh") {
          refreshBoard();
        } else if (ev.type === "error") {
          error = ev.message;
        }
        scrollAgent();
      };

      try {
        await streamTurn(url, data, handle);
      } catch (err) {
        error = error || err.message;
      }

      // Swap the streamed text for the saved, Markdown-rendered conversation.
      try {
        await htmx.ajax("GET", `/boards/${boardId}/agent/history`, { target: "#agent-messages", swap: "innerHTML" });
      } catch (_) {
        status.remove();
      }
      if (error) errorLine(error);
      scrollAgent();
      refreshBoard();
      busy = false;
      sendBtn.disabled = false;
    }

    // ---- "Send To Agent": one tab per run in the dock at the bottom of the board ------------

    const dock = $("#agent-dock");
    const dockTabs = $("#agent-dock-tabs");
    const dockConsole = $("#agent-dock-console");
    let shown = null; // the run whose console is open

    function showRun(run) {
      shown = run;
      dockConsole.classList.toggle("hidden", !run);
      for (const t of dockTabs.children) t.classList.toggle("btn-active", t === run?.tab);
      for (const l of dockConsole.children) l.classList.toggle("hidden", l !== run?.log);
      if (run) dockConsole.scrollTop = dockConsole.scrollHeight;
    }

    async function sendColumn(columnId, name, cardCount) {
      busy = true;
      sendBtn.disabled = true;
      const run = { done: false, abort: new AbortController() };
      run.log = el("div", "hidden flex flex-col gap-0.5");
      run.tab = el("button", "btn btn-xs gap-1 shrink-0");
      run.tab.type = "button";
      const icon = el("span", "loading loading-spinner loading-xs");
      const close = el("span", "opacity-50 hover:opacity-100 pl-1", "✕");
      close.title = "Stop";
      run.tab.append(icon, el("span", "max-w-40 truncate", `✦ ${name}`), close);
      run.tab.title = "Show what the agent is doing";
      dockTabs.append(run.tab);
      dockConsole.append(run.log);
      dock.classList.remove("hidden");

      // Keep the console pinned to the newest line unless the reader has scrolled up.
      const follow = (change) => {
        const pinned = dockConsole.scrollHeight - dockConsole.scrollTop - dockConsole.clientHeight < 40;
        const out = change();
        if (shown === run && pinned) dockConsole.scrollTop = dockConsole.scrollHeight;
        return out;
      };
      const line = (text, cls = "") =>
        follow(() => {
          const row = el("div", `flex gap-2 ${cls}`);
          const body = el("span", "whitespace-pre-wrap break-words min-w-0", text);
          row.append(el("span", "opacity-50 shrink-0", new Date().toLocaleTimeString()), body);
          run.log.append(row);
          return body;
        });

      run.tab.addEventListener("click", (e) => {
        if (e.target !== close) return showRun(shown === run ? null : run);
        if (!run.done) {
          if (confirm(`Stop the agent working on “${name}”? Changes it has already made are kept.`)) run.abort.abort();
          return;
        }
        if (shown === run) showRun(null);
        run.tab.remove();
        run.log.remove();
        if (!dockTabs.children.length) dock.classList.add("hidden");
      });

      line(`Sent “${name}” to the agent (${cardCount} card${cardCount === 1 ? "" : "s"})`);
      let live = null; // the line the agent's current message is streaming into
      let error = null;
      try {
        await streamTurn(
          `/columns/${columnId}/agent`,
          null,
          (ev) => {
            if (ev.type === "delta") {
              if (!live) live = line("");
              follow(() => (live.textContent += ev.text));
            } else if (ev.type === "tool") {
              live = null;
              line(`${ev.ok ? "✓" : "✗"} ${ev.name} ${ev.arguments}${ev.error ? ` — ${ev.error}` : ""}`, ev.ok ? "opacity-80" : "text-error");
            } else if (ev.type === "meta") {
              line(`Model: ${ev.model}`, "opacity-60");
            } else if (ev.type === "refresh") {
              refreshBoard();
            } else if (ev.type === "error") {
              error = ev.message;
            }
          },
          run.abort.signal
        );
      } catch (err) {
        error = error || (run.abort.signal.aborted ? "Stopped" : err.message);
      }

      run.done = true;
      line(error ? `Failed: ${error}` : "Finished", error ? "text-error" : "text-success");
      icon.className = error ? "text-error" : "text-success";
      icon.textContent = error ? "✗" : "✓";
      close.title = "Close";
      if (error && shown !== run) toast(`Send To Agent · ${name}: ${error}`);
      htmx.trigger(document.body, "agent-history"); // the run is also part of the saved conversation
      refreshBoard();
      busy = false;
      sendBtn.disabled = false;
    }

    form.addEventListener("submit", (e) => {
      e.preventDefault();
      const message = input.value.trim();
      if (!message || busy) return;
      input.value = "";
      run(`/boards/${boardId}/agent/chat`, { message }, message);
    });
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
        e.preventDefault();
        form.requestSubmit();
      }
    });

    document.addEventListener("click", (e) => {
      if (e.target.closest("[data-agent-toggle]")) setOpen(panel.classList.contains("hidden"));
      if (e.target.closest("[data-agent-reset]") && !busy && confirm("Clear this conversation with the agent?")) {
        post(`/boards/${boardId}/agent/reset`)
          .then(() => htmx.trigger(document.body, "agent-history"))
          .catch((err) => toast(err.message));
      }
      const send = e.target.closest("[data-send-column]");
      if (send) {
        if (busy) return toast("The agent is still working on the previous request", "warning");
        const column = send.closest(".column");
        sendColumn(send.dataset.sendColumn, column?.querySelector("h3")?.textContent || "column", column?.querySelectorAll(".card-item").length || 0);
      }
    });
  }
})();
