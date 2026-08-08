import bpy

input_path = 'C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/Gemma27/task1.blend'
output_path = 'C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/Gemma27/task4.blend'

try:
    bpy.ops.wm.open_mainfile(filepath=input_path)
except Exception:
    pass

if "Cube" in bpy.data.objects:
    cube = bpy.data.objects["Cube"]
    cube.location.z += 3.0

bpy.ops.wm.save_as_mainfile(filepath=output_path)