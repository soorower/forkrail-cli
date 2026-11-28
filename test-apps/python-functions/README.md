# python-functions

Vercel's zero-config Python functions beside static files: every `.py` file in `api/` that serves
something (a `handler` class, or an ASGI or WSGI `app`) is a function. forkrail runs them all in
one Python app (the runtime's shim dispatches by the router's routes). With FastAPI in its
requirements and an `api/index.py`, a project with one function would be the FastAPI preset; with
several in `api/`, each is its own function.

- `GET /`: `public/index.html`
- `GET /api`, `POST /api` (`api/index.py`): a `BaseHTTPRequestHandler` class, which sees the raw
  path, and a module beside it (`_utils.py`, not a function)
- `GET /api/items/42` (`api/items/[id].py`): a FastAPI app, which sees the visitor's path and `id`
  in the query
- `GET /api/flask_app?x=1`, `POST /api/flask_app` (form): a Flask (WSGI) app
- `GET /api/cron`: the cron (`*/5 * * * *`); 401 without `Authorization: Bearer $CRON_SECRET`
- `GET /api/helpers`, `GET /api/_utils`: 404, not functions
