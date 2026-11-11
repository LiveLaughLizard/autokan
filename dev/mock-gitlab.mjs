// In-memory imitation of the parts of the GitLab REST API that Autokan's board sync uses.
// NOT GitLab: it exists so the sync can be exercised without an account.
//
//   node dev/mock-gitlab.mjs          # http://localhost:9300 , token "glpat-test", project "group/proj"
//
// Link a board with: service GitLab, server URL http://localhost:9300, token glpat-test, project group/proj.
// GET /_state shows the whole board. Set NO_DELETE=1 to refuse issue deletion (as for non-owners).
import http from "node:http";

const PORT = Number(process.env.PORT || 9300);
const TOKEN = "glpat-test";
let nextId = 100;
const labels = [{ id: 1, name: "To Do", color: "#f00" }, { id: 2, name: "Doing", color: "#0f0" }];
const boards = [{ id: 1, name: "Development", lists: [{ id: 11, label_id: 1, position: 0 }, { id: 12, label_id: 2, position: 1 }] }];
const issues = [
  { id: 1001, iid: 1, title: "Remote issue one", description: "From GitLab", state: "opened", labels: ["To Do"], pos: 10 },
  { id: 1002, iid: 2, title: "Remote issue two", description: "", state: "opened", labels: [], pos: 20 },
  { id: 1003, iid: 3, title: "Old closed issue", description: "", state: "closed", labels: ["Doing"], pos: 30 },
];
const pub = (i) => ({ id: i.id, iid: i.iid, title: i.title, description: i.description, state: i.state, labels: i.labels });
const listOut = (l) => ({ id: l.id, position: l.position, label: labels.find((x) => x.id === l.label_id) });
const csv = (v) => (Array.isArray(v) ? v : String(v ?? "").split(",")).map((s) => s.trim()).filter(Boolean);

function send(res, status, body, headers = {}) {
  res.writeHead(status, { "content-type": "application/json", ...headers });
  res.end(body === undefined ? "" : JSON.stringify(body));
}

http
  .createServer((req, res) => {
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => {
      const url = new URL(req.url, `http://localhost:${PORT}`);
      if (url.pathname === "/_state") {
        const b = boards[0];
        const lists = [...b.lists].sort((x, y) => x.position - y.position).map((l) => labels.find((x) => x.id === l.label_id).name);
        const column = (i) => (i.state === "closed" ? "Closed" : lists.find((n) => i.labels.includes(n)) || "Open");
        const out = {};
        for (const name of ["Open", ...lists, "Closed"]) out[name] = [...issues].sort((x, y) => x.pos - y.pos).filter((i) => column(i) === name).map((i) => i.title);
        return send(res, 200, out);
      }
      if (req.headers["private-token"] !== TOKEN) return send(res, 401, { message: "401 Unauthorized" });
      const body = raw ? JSON.parse(raw) : {};
      const m = url.pathname.match(/^\/api\/v4\/projects\/([^/]+)(\/.*)?$/);
      if (!m || decodeURIComponent(m[1]) !== "group/proj") return send(res, 404, { message: "404 Project Not Found" });
      const path = m[2] || "";
      const M = req.method;
      let x;
      if (path === "/boards" && M === "GET") return send(res, 200, boards.map((b) => ({ id: b.id, name: b.name })));
      if (path === "/boards" && M === "POST") { const b = { id: nextId++, name: body.name, lists: [] }; boards.push(b); return send(res, 201, { id: b.id, name: b.name }); }
      if ((x = path.match(/^\/boards\/(\d+)\/lists(?:\/(\d+))?$/))) {
        const board = boards.find((b) => b.id === Number(x[1]));
        if (!board) return send(res, 404, { message: "404 Board Not Found" });
        if (!x[2] && M === "GET") return send(res, 200, board.lists.map(listOut));
        if (!x[2] && M === "POST") {
          if (!labels.some((l) => l.id === body.label_id)) return send(res, 400, { message: "label not found" });
          const l = { id: nextId++, label_id: body.label_id, position: board.lists.length };
          board.lists.push(l);
          return send(res, 201, listOut(l));
        }
        const list = board.lists.find((l) => l.id === Number(x[2]));
        if (!list) return send(res, 404, { message: "404 List Not Found" });
        if (M === "PUT") {
          const ordered = [...board.lists].sort((a, b) => a.position - b.position).filter((l) => l !== list);
          ordered.splice(body.position, 0, list);
          ordered.forEach((l, i) => (l.position = i));
          return send(res, 200, listOut(list));
        }
        if (M === "DELETE") { board.lists = board.lists.filter((l) => l !== list); return send(res, 204); }
      }
      if (path === "/labels" && M === "GET") { const q = (url.searchParams.get("search") || "").toLowerCase(); return send(res, 200, labels.filter((l) => l.name.toLowerCase().includes(q))); }
      if (path === "/labels" && M === "POST") {
        if (labels.some((l) => l.name.toLowerCase() === body.name.toLowerCase())) return send(res, 409, { message: "Label already exists" });
        const l = { id: nextId++, name: body.name, color: body.color };
        labels.push(l);
        return send(res, 201, l);
      }
      if ((x = path.match(/^\/labels\/(\d+)$/)) && M === "PUT") {
        const l = labels.find((l) => l.id === Number(x[1]));
        if (!l) return send(res, 404, { message: "404 Label Not Found" });
        for (const i of issues) i.labels = i.labels.map((n) => (n === l.name ? body.new_name : n));
        l.name = body.new_name;
        return send(res, 200, l);
      }
      if (path === "/issues" && M === "GET") {
        const per = Number(url.searchParams.get("per_page") || 20), page = Number(url.searchParams.get("page") || 1);
        const all = issues.filter((i) => i.state === (url.searchParams.get("state") || "opened")).sort((a, b) => a.pos - b.pos);
        const slice = all.slice((page - 1) * per, page * per);
        return send(res, 200, slice.map(pub), { "x-next-page": page * per < all.length ? String(page + 1) : "" });
      }
      if (path === "/issues" && M === "POST") {
        const i = { id: nextId++, iid: Math.max(0, ...issues.map((i) => i.iid)) + 1, title: body.title, description: body.description || "", state: "opened", labels: csv(body.labels), pos: Math.max(0, ...issues.map((i) => i.pos)) + 10 };
        issues.push(i);
        return send(res, 201, pub(i));
      }
      if ((x = path.match(/^\/issues\/(\d+)(\/reorder)?$/))) {
        const i = issues.find((i) => i.iid === Number(x[1]));
        if (!i) return send(res, 404, { message: "404 Not found" });
        if (x[2] && M === "PUT") {
          const before = issues.find((o) => o.id === body.move_before_id), after = issues.find((o) => o.id === body.move_after_id);
          if (before && after) i.pos = (before.pos + after.pos) / 2;
          else if (before) i.pos = before.pos + 0.001 * Math.random() + Math.min(5, ...issues.filter((o) => o.pos > before.pos && o !== i).map((o) => (o.pos - before.pos) / 2));
          else if (after) i.pos = after.pos - Math.min(5, ...issues.filter((o) => o.pos < after.pos && o !== i).map((o) => (after.pos - o.pos) / 2));
          return send(res, 200, pub(i));
        }
        if (M === "GET") return send(res, 200, pub(i));
        if (M === "PUT") {
          if (body.title !== undefined) i.title = body.title;
          if (body.description !== undefined) i.description = body.description;
          if (body.remove_labels !== undefined) i.labels = i.labels.filter((l) => !csv(body.remove_labels).includes(l));
          if (body.add_labels !== undefined) for (const l of csv(body.add_labels)) if (!i.labels.includes(l)) i.labels.push(l);
          if (body.labels !== undefined) i.labels = csv(body.labels);
          if (body.state_event === "close") i.state = "closed";
          if (body.state_event === "reopen") i.state = "opened";
          return send(res, 200, pub(i));
        }
        if (M === "DELETE") {
          if (process.env.NO_DELETE) return send(res, 403, { message: "403 Forbidden" });
          issues.splice(issues.indexOf(i), 1);
          return send(res, 204);
        }
      }
      send(res, 404, { message: `404 mock has no ${M} ${path}` });
    });
  })
  .listen(PORT, () => console.log(`mock GitLab on http://localhost:${PORT} (token ${TOKEN}, project group/proj)`));
