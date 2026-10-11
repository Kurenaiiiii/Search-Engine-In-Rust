# API reference

Base URL: `http://127.0.0.1:3000`. All examples below are real calls against
the demo index (see README quick start). Errors are JSON: `{"error":"..."}`.

Auth: if the server was started with `SE_TOKEN` set, every `/api/*` route
needs `Authorization: Bearer <token>` (401 otherwise). `/`, `/health` and
`/ready` stay public.

## Health

```bash
curl localhost:3000/health
# {"ok":true}
curl localhost:3000/ready
# {"ok":true}
```

## Add a document

```bash
curl -X POST localhost:3000/api/docs -d '{"title":"Hello","body":"hello world search"}'
# {"id":0}
```

## Bulk index (NDJSON or JSON array, max 100k docs / 8 MiB body)

```bash
curl -X POST localhost:3000/api/docs/bulk --data-binary @docs.ndjson
# {"added":[1,2,3]}
```

## Get / update / delete

```bash
curl localhost:3000/api/docs/1
# {"id":1,"fields":{"body":"...","title":"..."}}

curl -X PUT localhost:3000/api/docs/1 -d '{"title":"Hello2","body":"updated body text"}'
# {"id":4}   # update = tombstone + new id

curl -X DELETE localhost:3000/api/docs/1
# {"deleted":true}
```

## Search

```bash
curl 'localhost:3000/api/search?q=rust&k=5&highlight=1'
# {"total":3,"took_ms":0,"hits":[
#   {"id":3,"score":1.0569,
#    "fields":{"body":"Learn Rust ownership borrowing and lifetimes","title":"Rust programming"},
#    "snippet":"Learn <mark>Rust</mark> ownership borrowing and lifetimes"},
#   ...]}
```

Parameters: `q` (required, max 4096 chars), `k` (default 10, max 100),
`offset`, `fields=title,body`, `highlight=1`, `scorer=tfidf`.
POST also works with `{"q":"..."}`. Unknown `q` syntax returns 400 with the
parser message, e.g. `{"error":"unterminated quoted phrase: missing closing \""}`.
Single-term queries with zero hits may include `"did_you_mean":"..."`.

Query language: `AND OR NOT`, parentheses, `"quoted phrases"`, `field:term`,
`prefix*`, `term~N` (N = 0..2), `term^boost`. Juxtaposition is AND.

## Suggest / spellcheck

```bash
curl 'localhost:3000/api/suggest?prefix=sea'
# {"suggestions":["search"]}
curl 'localhost:3000/api/spell?term=serch'
# {"suggestion":"search"}
```

Optional `field` (default `body`) and `limit` (default 8, max 50).

## Stats

```bash
curl localhost:3000/api/stats
# {"live_docs":4,"segments":2,"buffered_docs":0,"postings_bytes":322,"dict_bytes":206,"next_id":4}
```
