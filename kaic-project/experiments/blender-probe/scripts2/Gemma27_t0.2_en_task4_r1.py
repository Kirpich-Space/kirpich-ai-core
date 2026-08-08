import bpy

input_path = "C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Gemma27_t0.2_en_r1/task1.blend"
output_path = "C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Gemma27_t0.2_en_r1/task4.blend"

bpy.ops.wm.open_mainfile(filepath=input_path)

if "Cube" in bpy.data.objects:
    cube = bpy.data.objects["Cube"]
    cube.location.z += 3

bpy.ops.wm.save_as_mainfile(filepath=output_path)