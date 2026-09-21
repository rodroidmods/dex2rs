.class public Lcom/example/InvokeConst;
.super Ljava/lang/Object;

.field public static s:I

.field public instance:I

.method public static methodTypeConstant()Ljava/lang/invoke/MethodType;
    .registers 1

    const-method-type v0, (ILjava/lang/String;)V

    return-object v0
.end method

.method public static voidMethodType()Ljava/lang/invoke/MethodType;
    .registers 1

    const-method-type v0, ()V

    return-object v0
.end method

.method public static staticGetter()Ljava/lang/invoke/MethodHandle;
    .registers 1

    const-method-handle v0, "static-get@Lcom/example/InvokeConst;->s:I"

    return-object v0
.end method

.method public static staticSetter()Ljava/lang/invoke/MethodHandle;
    .registers 1

    const-method-handle v0, "static-put@Lcom/example/InvokeConst;->s:I"

    return-object v0
.end method

.method public static instanceGetter()Ljava/lang/invoke/MethodHandle;
    .registers 1

    const-method-handle v0, "instance-get@Lcom/example/InvokeConst;->instance:I"

    return-object v0
.end method

.method public static instanceSetter()Ljava/lang/invoke/MethodHandle;
    .registers 1

    const-method-handle v0, "instance-put@Lcom/example/InvokeConst;->instance:I"

    return-object v0
.end method

.method public static staticMethodHandle()Ljava/lang/invoke/MethodHandle;
    .registers 1

    const-method-handle v0, "invoke-static@Ljava/lang/Math;->abs(I)I"

    return-object v0
.end method

.method public static virtualMethodHandle()Ljava/lang/invoke/MethodHandle;
    .registers 1

    const-method-handle v0, "invoke-instance@Ljava/lang/String;->length()I"

    return-object v0
.end method

.method public static interfaceMethodHandle()Ljava/lang/invoke/MethodHandle;
    .registers 1

    const-method-handle v0, "invoke-interface@Ljava/lang/Runnable;->run()V"

    return-object v0
.end method

.method public static constructorHandle()Ljava/lang/invoke/MethodHandle;
    .registers 1

    const-method-handle v0, "invoke-constructor@Ljava/lang/StringBuilder;-><init>()V"

    return-object v0
.end method

.method public static directMethodHandle()Ljava/lang/invoke/MethodHandle;
    .registers 1

    const-method-handle v0, "invoke-direct@Lcom/example/InvokeConst;-><init>()V"

    return-object v0
.end method
