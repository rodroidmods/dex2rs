.class public Lcom/example/Hello;
.super Ljava/lang/Object;

.field public static counter:I

.method public instanceOnly()I
    .registers 1

    const/4 v0, 0x5

    return v0
.end method

.method public static add(II)I
    .registers 3

    add-int v0, p0, p1

    return v0
.end method

.method public static max(II)I
    .registers 3

    if-ge p0, p1, :cond_0

    move v0, p0

    goto :done

    :cond_0
    move v0, p1

    :done
    return v0
.end method

.method public static sum(I)I
    .registers 4

    const/4 v0, 0x0

    const/4 v1, 0x0

    :loop
    if-ge v1, p0, :end

    add-int/2addr v0, v1

    add-int/lit8 v1, v1, 0x1

    goto :loop

    :end
    return v0
.end method

.method public static greet()Ljava/lang/String;
    .registers 1

    const-string v0, "hello"

    return-object v0
.end method

.method public static wide(J)J
    .registers 4

    const-wide/16 v0, 0x2

    mul-long v0, p0, v0

    return-wide v0
.end method

.method public static divZero(I)I
    .registers 3

    const/4 v0, 0x0

    :try_start
    div-int v1, p0, v0
    :try_end
    return v1

    .catch Ljava/lang/ArithmeticException; {:try_start .. :try_end} :handler

    :handler
    const/4 v1, -0x1

    return v1
.end method

.method public static makeArray()[I
    .registers 3

    const/4 v0, 0x3

    new-array v1, v0, [I

    const/4 v2, 0x0

    const/4 v0, 0x7

    aput v0, v1, v2

    return-object v1
.end method

.method public static dataArray()[I
    .registers 2

    const/4 v0, 0x3

    new-array v1, v0, [I

    fill-array-data v1, :data

    return-object v1

    :data
    .array-data 4
        1
        2
        3
    .end array-data
.end method

.method public static bump()V
    .registers 2

    sget v0, Lcom/example/Hello;->counter:I

    add-int/lit8 v0, v0, 0x1

    sput v0, Lcom/example/Hello;->counter:I

    return-void
.end method

.method public static callObject(Ljava/lang/Object;)I
    .registers 2

    invoke-virtual {p0}, Ljava/lang/Object;->hashCode()I

    move-result v0

    return v0
.end method

.method public static deadResult(Ljava/lang/Object;)Ljava/lang/Object;
    .registers 2

    invoke-virtual {p0}, Ljava/lang/Object;->toString()Ljava/lang/String;

    return-object p0
.end method

.method public static check(Ljava/lang/Object;)Z
    .registers 2

    instance-of v0, p0, Ljava/lang/String;

    return v0
.end method

.method public static classify(I)I
    .locals 1

    packed-switch p0, :pswitch_data

    const/16 v0, 0x63

    return v0

    :pswitch_0
    const/4 v0, 0x1

    return v0

    :pswitch_1
    const/4 v0, 0x2

    return v0

    :pswitch_data
    .packed-switch 0x0
        :pswitch_0
        :pswitch_1
    .end packed-switch
.end method

.method public static kind()Ljava/lang/Class;
    .registers 1

    const-class v0, Ljava/lang/String;

    return-object v0
.end method

.method public static boom(Ljava/lang/Throwable;)V
    .registers 1

    throw p0
.end method

.method public static escaped()Ljava/lang/String;
    .registers 1

    const-string v0, "a\nb\tc\"d\\e\u00e9"

    return-object v0
.end method

.method public static tick()I
    .registers 1

    const/4 v0, 0x7

    return v0
.end method

# A `synchronized` body whose monitor register is reused for a `long` *after*
# the last instruction in the protected range that can throw. The handler must
# still see the monitor object: the `int-to-long` cannot transfer control, so a
# value it writes can never reach the handler. (This is the shape of
# `FileChannelDataSource;->copyTo` in apksig, where the leaked `long` reached a
# `monitor-exit` and the handler's phi became inconsistent.)
.method public static reuseAfterThrow()V
    .registers 3

    new-instance v0, Ljava/lang/Object;

    invoke-direct {v0}, Ljava/lang/Object;-><init>()V

    :try_start
    monitor-enter v0

    invoke-static {}, Lcom/example/Hello;->tick()I

    move-result v1

    monitor-exit v0

    int-to-long v0, v1

    add-long/2addr v0, v0
    :try_end
    return-void

    .catch Ljava/lang/Exception; {:try_start .. :try_end} :handler

    :handler
    monitor-exit v0

    return-void
.end method
