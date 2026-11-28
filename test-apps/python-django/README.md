# python-django

Django as Vercel's Django preset runs it: the entry from `manage.py`'s settings
(`WSGI_APPLICATION`), its static files collected at build time (`STATIC_ROOT` is set, with
`ManifestStaticFilesStorage`) and served from `/static/`. Its dependencies are in
`pyproject.toml`, whose `requires-python = "==3.12.*"` asks for Python 3.12.

- `GET /`: the framework, Python version and the hashed URL of `hello/site.css`
- `GET /items/7`: a route with a parameter
- `GET /static/hello/site.css`: a collected static file, served by the router
