import sys

def fib(n):
    a, b = 0, 1
    for _ in range(n):
        a, b = b, a + b
    return a

if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("usage: fib.py <n>")
        sys.exit(1)
    n = int(sys.argv[1])
    print(fib(n))
