def foo(a, b, c):
    return a + b + c

# array = Array(FunctionDef(foo))
array = FunctionArray(foo, env=dict(FOO=BAR), name="foo")
for i in range(10):
    array.entry((1, 2), c=i)

# int range -> str name map
# Task 500 => name foo[100]
array.map([[1, 2], [3, 4]])
array.map([
    args(1, 2, c=3),
    call(1, 2, d=3)
])
array.map(args=[[1, 2], [3, 4]], kwargs=[dict(c=3), dict(d=3)])

array = CommandArray(["bash", "-c", "ls"], env=dict(FOO=BAR))
array.empty_entries(1000)

# Dependencies?
graph = Graph()
# submit.add_task(Command(["ls"]))
t1 = graph.task(Command(["ls"]))
# Assert that all deps are from the same graph
t2 = graph.task(Function(foo, args=(1, 2), kwargs=dict(c=3)), deps=(t1, ))

# Automatically share/compact shared data
# Just Task, create it from Python
# Task properties builder
# Task resources builder

client = HqClient()
# Simple submit
job = client.submit(submit)

# Open job
job = client.open_job()
job.submit(submit)
job.close()
job.wait(wait_for_close=False)

# Ctrl + C => turn off local cluster
# with block for job
