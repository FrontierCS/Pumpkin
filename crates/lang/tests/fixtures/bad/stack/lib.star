"""The file measured to take the most stack: a function's comprehensions, nested one less deep
than the language takes, each inside every binary operator and a conditional, with, at the
innermost, a call that checks a value nested 127 deep against a type as deep, and so runs its body
as deep as the language takes, then `==` on a value nested 128 deep, where `+` then refuses an int
and a bool."""

load("//lib/deep.star", "DEEP")

def t(x: list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[
    list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[
    list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[
    list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[
    list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[
    list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[
    list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[
    list[list[list[list[list[list[list[list[list[list[list[list[list[list[list[
    int
    ]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]
    ]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]) -> int:
    return 1

def deepest():
    return 0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        0 or 1 and 0 == {} | 0 + 0 * [0 or 1 and 0 == {} | 0 + 0 * [
        t(INNER) + (DEEP == DEEP)
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0
        for _ in Z][0] if True else 0 for _ in Z][0] if True else 0

Z = [0]

INNER = DEEP[0]

DEEPEST = deepest()
