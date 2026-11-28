from fastapi import FastAPI, Request

app = FastAPI()


# The app sees the path the visitor asked for, and the dynamic segment in the query.
@app.get("/api/items/{item}")
async def item(item: str, request: Request):
    return {"function": "api/items/[id].py", "item": item, "id": request.query_params.get("id")}
