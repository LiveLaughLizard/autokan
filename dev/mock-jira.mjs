// In-memory imitation of the parts of the Jira REST API (version 2) that Autokan uses.
// NOT Jira: it exists so the integration can be exercised without an account.
//
//   node dev/mock-jira.mjs                 # http://localhost:9500 , project ABC
//   BASE_PATH=/jira node dev/mock-jira.mjs # as a self-hosted Jira served under a context path
//
// Accepts either sign-in method Autokan offers:
//   email "jira@example.com" + API token "jira-token"   (basic auth, as on Jira Cloud)
//   personal access token "jira-pat"                    (bearer, as on self-hosted Jira)
// Workflow: To Do -> In Progress -> Done (and back). GET /_state dumps every issue.
import http from "node:http";

const PORT = Number(process.env.PORT || 9500);
const BASE = (process.env.BASE_PATH || "").replace(/\/$/, "");
const BASIC = "Basic " + Buffer.from("jira@example.com:jira-token").toString("base64");
const BEARER = "Bearer jira-pat";
const WORKFLOW = { "To Do": ["In Progress"], "In Progress": ["To Do", "Done"], Done: ["In Progress"] };
const issues = new Map();
let seq = 0;

http
  .createServer((req, res) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const raw = Buffer.concat(chunks);
      const send = (status, body) => res.writeHead(status, { "content-type": "application/json" }).end(body === undefined ? "" : JSON.stringify(body));
      const url = new URL(req.url, `http://localhost:${PORT}`);
      if (url.pathname === "/_state") return send(200, [...issues.values()]);
      if (!url.pathname.startsWith(`${BASE}/rest/api/2/`)) return send(404, { errorMessages: ["dead link"] });
      if (![BASIC, BEARER].includes(req.headers.authorization)) return send(401, { errorMessages: ["You are not authenticated."] });
      const path = url.pathname.slice(`${BASE}/rest/api/2`.length);
      const M = req.method;
      const json = () => (raw.length ? JSON.parse(raw.toString()) : {});
      let m;
      if (path === "/myself") return send(200, { displayName: "Jira Bot", name: "bot" });
      if ((m = path.match(/^\/project\/(\w+)$/))) return m[1] === "ABC" ? send(200, { key: "ABC", name: "Alphabet", issueTypes: [{ name: "Task" }, { name: "Bug" }] }) : send(404, { errorMessages: [`No project could be found with key '${m[1]}'.`] });
      if (path === "/issue" && M === "POST") {
        const f = json().fields || {};
        if (f.project?.key !== "ABC") return send(400, { errors: { project: "project is required" } });
        if (!f.summary) return send(400, { errors: { summary: "You must specify a summary of the issue." } });
        if (!["Task", "Bug"].includes(f.issuetype?.name)) return send(400, { errors: { issuetype: "issue type is required" } });
        const key = `ABC-${++seq}`;
        issues.set(key, { key, type: f.issuetype.name, summary: f.summary, description: f.description || "", status: "To Do", comments: [], attachments: [] });
        return send(201, { id: String(10000 + seq), key });
      }
      if ((m = path.match(/^\/issue\/([A-Z]+-\d+)(\/.*)?$/))) {
        const issue = issues.get(m[1]);
        if (!issue) return send(404, { errorMessages: ["Issue Does Not Exist"] });
        const rest = m[2] || "";
        if (rest === "" && M === "GET") return send(200, { key: issue.key, fields: { status: { name: issue.status }, summary: issue.summary } });
        if (rest === "" && M === "PUT") {
          const f = json().fields || {};
          if (f.summary !== undefined) issue.summary = f.summary;
          if (f.description !== undefined) issue.description = f.description;
          return send(204);
        }
        if (rest === "/transitions" && M === "GET") return send(200, { transitions: WORKFLOW[issue.status].map((to) => ({ id: to.replace(/ /g, "_"), name: `Move to ${to}`, to: { name: to } })) });
        if (rest === "/transitions" && M === "POST") {
          const to = String(json().transition?.id || "").replace(/_/g, " ");
          if (!WORKFLOW[issue.status].includes(to)) return send(400, { errorMessages: ["Transition is not valid for this issue."] });
          issue.status = to;
          return send(204);
        }
        if (rest === "/comment" && M === "POST") { const c = { id: String(20000 + ++seq), body: json().body }; issue.comments.push(c); return send(201, c); }
        let c;
        if ((c = rest.match(/^\/comment\/(\d+)$/))) {
          const comment = issue.comments.find((x) => x.id === c[1]);
          if (!comment) return send(404, { errorMessages: ["Can not find a comment for the id."] });
          if (M === "PUT") { comment.body = json().body; return send(200, comment); }
          if (M === "DELETE") { issue.comments = issue.comments.filter((x) => x !== comment); return send(204); }
        }
        if (rest === "/attachments" && M === "POST") {
          if (req.headers["x-atlassian-token"] !== "no-check") return send(403, { errorMessages: ["XSRF check failed"] });
          const boundary = (req.headers["content-type"] || "").match(/boundary=(.+)$/)?.[1];
          if (!boundary) return send(400, { errorMessages: ["expected multipart/form-data"] });
          const text = raw.toString("latin1");
          const added = [];
          for (const part of text.split(`--${boundary}`)) {
            const name = part.match(/name="file"; filename="([^"]*)"/)?.[1];
            if (!name) continue;
            const size = Buffer.byteLength(part.split("\r\n\r\n").slice(1).join("\r\n\r\n").replace(/\r\n$/, ""), "latin1");
            issue.attachments.push({ filename: name, size });
            added.push({ filename: name, size });
          }
          return added.length ? send(200, added) : send(400, { errorMessages: ["no file part"] });
        }
      }
      send(404, { errorMessages: [`mock-jira has no ${M} ${path}`] });
    });
  })
  .listen(PORT, () => console.log(`mock Jira on http://localhost:${PORT}${BASE} (project ABC; jira@example.com / jira-token, or PAT jira-pat)`));
