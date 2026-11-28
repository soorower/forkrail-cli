// Not a function: files and folders starting with _ aren't, on Vercel.
package lib

func Greeting() string { return "hi from api/_lib" }
