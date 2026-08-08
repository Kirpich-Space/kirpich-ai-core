

bpy.ops.wm.open_main_file(filepath=r'C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/Fable9B/task1.blend')
cube = [obj for obj in bpy.data.objects if obj.name == 'Cube']
if not cube: raise RuntimeError('Cube not found')
for c in cube: c.location.z += 3.0
bpy.ops.wm.save_as_mainfile(filepath=r'C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/Fable9B/task4.blend')