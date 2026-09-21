.class public Lcom/example/Polymorphic;
.super Ljava/lang/Object;

.method public static handleCall(Ljava/lang/invoke/MethodHandle;II)I
    .registers 4

    invoke-polymorphic {p0, p1, p2}, Ljava/lang/invoke/MethodHandle;->invokeExact([Ljava/lang/Object;)Ljava/lang/Object;, (II)I

    move-result p0

    return p0
.end method

.method public static handleRange(Ljava/lang/invoke/MethodHandle;II)I
    .registers 4

    invoke-polymorphic/range {p0 .. p2}, Ljava/lang/invoke/MethodHandle;->invokeExact([Ljava/lang/Object;)Ljava/lang/Object;, (II)I

    move-result p0

    return p0
.end method

.method public static varHandleCall(Ljava/lang/invoke/VarHandle;[III)I
    .registers 5

    invoke-polymorphic {p0, p1, p2, p3}, Ljava/lang/invoke/VarHandle;->getAndAdd([Ljava/lang/Object;)Ljava/lang/Object;, ([III)I

    move-result p0

    return p0
.end method

.method public static varHandleVoid(Ljava/lang/invoke/VarHandle;[II)V
    .registers 4

    invoke-polymorphic {p0, p1, p2}, Ljava/lang/invoke/VarHandle;->set([Ljava/lang/Object;)Ljava/lang/Object;, ([II)V

    return-void
.end method

.method public static handleObject(Ljava/lang/invoke/MethodHandle;Ljava/lang/String;)Ljava/lang/String;
    .registers 3

    invoke-polymorphic {p0, p1}, Ljava/lang/invoke/MethodHandle;->invoke([Ljava/lang/Object;)Ljava/lang/Object;, (Ljava/lang/String;)Ljava/lang/String;

    move-result-object p0

    return-object p0
.end method

.method public static handleWide(Ljava/lang/invoke/MethodHandle;JII)J
    .registers 7

    invoke-polymorphic {p0, p1, p2, p3, p4}, Ljava/lang/invoke/MethodHandle;->invokeExact([Ljava/lang/Object;)Ljava/lang/Object;, (JII)J

    move-result-wide p0

    return-wide p0
.end method
