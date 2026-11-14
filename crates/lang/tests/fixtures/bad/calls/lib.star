"""Functions that call each other 49 deep, the innermost calling a method: `f1()` runs 48
functions at once, as many as the language runs, and `f0()` 49."""

def f0(x):
    return f1(x)

def f1(x):
    return f2(x)

def f2(x):
    return f3(x)

def f3(x):
    return f4(x)

def f4(x):
    return f5(x)

def f5(x):
    return f6(x)

def f6(x):
    return f7(x)

def f7(x):
    return f8(x)

def f8(x):
    return f9(x)

def f9(x):
    return f10(x)

def f10(x):
    return f11(x)

def f11(x):
    return f12(x)

def f12(x):
    return f13(x)

def f13(x):
    return f14(x)

def f14(x):
    return f15(x)

def f15(x):
    return f16(x)

def f16(x):
    return f17(x)

def f17(x):
    return f18(x)

def f18(x):
    return f19(x)

def f19(x):
    return f20(x)

def f20(x):
    return f21(x)

def f21(x):
    return f22(x)

def f22(x):
    return f23(x)

def f23(x):
    return f24(x)

def f24(x):
    return f25(x)

def f25(x):
    return f26(x)

def f26(x):
    return f27(x)

def f27(x):
    return f28(x)

def f28(x):
    return f29(x)

def f29(x):
    return f30(x)

def f30(x):
    return f31(x)

def f31(x):
    return f32(x)

def f32(x):
    return f33(x)

def f33(x):
    return f34(x)

def f34(x):
    return f35(x)

def f35(x):
    return f36(x)

def f36(x):
    return f37(x)

def f37(x):
    return f38(x)

def f38(x):
    return f39(x)

def f39(x):
    return f40(x)

def f40(x):
    return f41(x)

def f41(x):
    return f42(x)

def f42(x):
    return f43(x)

def f43(x):
    return f44(x)

def f44(x):
    return f45(x)

def f45(x):
    return f46(x)

def f46(x):
    return f47(x)

def f47(x):
    return f48(x)

def f48(x):
    return x.get(1)

DEEPEST = f1({1: 48})

TOO_DEEP = f0({1: 49})
