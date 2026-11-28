import sys

from django.http import JsonResponse
from django.templatetags.static import static


def index(request):
    print(f"django index {request.path}")
    return JsonResponse({"framework": "django", "python": sys.version.split()[0], "css": static("hello/site.css")})


def item(request, item):
    return JsonResponse({"item": item})
