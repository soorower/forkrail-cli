# Echoes what the app was asked for, so forkrail's router and Vercel's can be compared: which
# path and query a rewrite hands a function. Part of tools/routing-check.py's projects.

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse

app = FastAPI()


@app.api_route('/{rest:path}', methods=['GET', 'HEAD', 'POST', 'PUT', 'DELETE', 'PATCH', 'OPTIONS'])
async def echo(request: Request, rest: str):
    answer = {
        'method': request.method,
        'path': request.url.path,
        'query': request.url.query,
    }
    return JSONResponse(answer, headers={'x-own': 'app', 'cache-control': 'no-store'})
