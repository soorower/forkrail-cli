# A FastAPI app, laid out the way forkrail (and Vercel) run Python: `api/main.py` exports an ASGI
# `app`, with no server of its own. Add routes below, and packages to requirements.txt.

import os
import platform
from datetime import datetime, timezone

from fastapi import FastAPI

app = FastAPI(title="My API")


@app.get("/")
def home():
    return {
        "message": "Hello from FastAPI on forkrail",
        "python": platform.python_version(),
        "region": os.environ.get("AWS_REGION"),
        "time": datetime.now(timezone.utc).isoformat(),
    }


@app.get("/items/{item_id}")
def item(item_id: int, q: str | None = None):
    return {"item_id": item_id, "q": q}


@app.get("/health")
def health():
    return {"ok": True}
