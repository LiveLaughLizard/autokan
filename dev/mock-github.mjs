// In-memory imitation of the parts of GitHub's GraphQL API that Autokan's board sync uses
// (Projects v2). NOT GitHub: it only recognises the operations Autokan sends, by name.
//
//   node dev/mock-github.mjs          # http://localhost:9400/graphql , token "ghp-test"
//
// Link a board with: service GitHub, server URL http://localhost:9400/graphql, token ghp-test,
// owner my-org, project number 1. GET /_state shows the whole board.
// By default editing the Status options re-issues every option id and clears the items' status,
// the least forgiving behaviour a client has to survive. STABLE_IDS=1 keeps ids for unchanged names.
import http from "node:http";

const PORT = Number(process.env.PORT || 9400);
const TOKEN = "ghp-test";
let seq = 1;
const id = (p) => `${p}_${seq++}`;
const project = { id: "PVT_1", title: "Roadmap", fieldId: "PVTSSF_1" };
let options = ["Todo", "In Progress", "Done"].map((name) => ({ id: id("opt"), name, color: "GRAY", description: "" }));
const item = (kind, title, body, option) => ({ id: id("PVTI"), option, archived: false, content: { __typename: kind, id: id(kind === "DraftIssue" ? "DI" : "I"), title, body } });
let items = [
  item("DraftIssue", "Remote draft", "From GitHub", options[0].id),
  item("Issue", "Remote issue", "", options[1].id),
  item("DraftIssue", "Unsorted idea", "", null),
];

function execute(query, v) {
  const has = (name) => query.includes(name);
  if (has("organization(login")) {
    const found = v.login === "my-org" && v.number === 1 ? { projectV2: { id: project.id, title: project.title } } : null;
    return { data: { organization: found, user: null }, errors: [{ message: `Could not resolve to a User with the login of '${v.login}'.` }] };
  }
  if (has("node(id: $project)")) {
    if (v.project !== project.id) return { data: { node: null } };
    const per = Number(process.env.PAGE_SIZE || 100);
    const start = v.cursor ? Number(v.cursor) : 0;
    const page = items.slice(start, start + per);
    return {
      data: {
        node: {
          field: v.field === "Status" ? { id: project.fieldId, options } : null,
          items: {
            pageInfo: { hasNextPage: start + per < items.length, endCursor: String(start + per) },
            nodes: page.map((i) => ({ id: i.id, isArchived: i.archived, fieldValueByName: i.option ? { optionId: i.option } : null, content: i.content })),
          },
        },
      },
    };
  }
  const find = (itemId) => items.find((i) => i.id === itemId);
  const fail = (message) => ({ data: null, errors: [{ message }] });
  if (has("updateProjectV2Field(")) {
    const next = v.options.map((o) => {
      const same = options.find((x) => x.name === o.name);
      return { id: process.env.STABLE_IDS && same ? same.id : id("opt"), name: o.name, color: o.color, description: o.description };
    });
    for (const i of items) if (i.option && !next.some((o) => o.id === i.option)) i.option = null;
    options = next;
    return { data: { updateProjectV2Field: { projectV2Field: { options } } } };
  }
  if (has("addProjectV2DraftIssue(")) {
    const i = item("DraftIssue", v.title, v.body || "", null);
    items.push(i);
    return { data: { addProjectV2DraftIssue: { projectItem: { id: i.id, content: { id: i.content.id } } } } };
  }
  if (has("updateProjectV2DraftIssue(") || has("updateIssue(") || has("updatePullRequest(")) {
    const i = items.find((i) => i.content.id === v.id);
    if (!i) return fail("Could not resolve to a node with the global id");
    i.content.title = v.title;
    i.content.body = v.body ?? "";
    return { data: { ok: true } };
  }
  if (has("updateProjectV2ItemFieldValue(")) {
    const i = find(v.item);
    if (!i) return fail("Could not resolve to a ProjectV2Item");
    if (!options.some((o) => o.id === v.option)) return fail("The single select option Id does not belong to the field");
    i.option = v.option;
    return { data: { ok: true } };
  }
  if (has("clearProjectV2ItemFieldValue(")) {
    const i = find(v.item);
    if (!i) return fail("Could not resolve to a ProjectV2Item");
    i.option = null;
    return { data: { ok: true } };
  }
  if (has("updateProjectV2ItemPosition(")) {
    const i = find(v.item);
    if (!i) return fail("Could not resolve to a ProjectV2Item");
    items = items.filter((x) => x !== i);
    const at = v.after ? items.findIndex((x) => x.id === v.after) + 1 : 0;
    items.splice(at, 0, i);
    return { data: { ok: true } };
  }
  if (has("deleteProjectV2Item(")) {
    items = items.filter((i) => i.id !== v.item);
    return { data: { deleteProjectV2Item: { deletedItemId: v.item } } };
  }
  return fail("mock-github does not implement this operation");
}

http
  .createServer((req, res) => {
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => {
      const json = (status, body) => res.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(body));
      if (req.url === "/_state") {
        const out = { "No Status": items.filter((i) => !i.option && !i.archived).map((i) => i.content.title) };
        for (const o of options) out[o.name] = items.filter((i) => i.option === o.id && !i.archived).map((i) => i.content.title);
        return json(200, out);
      }
      if (req.url === "/_ids") return json(200, { options, items: items.map((i) => ({ id: i.id, content: i.content.id, title: i.content.title })) });
      if (req.headers.authorization !== `Bearer ${TOKEN}`) return json(401, { message: "Bad credentials" });
      const { query, variables } = JSON.parse(raw);
      json(200, execute(query, variables || {}));
    });
  })
  .listen(PORT, () => console.log(`mock GitHub on http://localhost:${PORT}/graphql (token ${TOKEN}, owner my-org, project 1)`));
