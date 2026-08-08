

import bpy
bpy.ops.object.select_all(action='DELETE')
for c in bpy.data.collections:
    if not c.objects: continue
del mesh
obj = bpy.data.meshes.new('Cube')
collection.objects.link(obj)
qsm = bpy.ops.quantumsmoothing
op = qsm.add({'object': obj})
mod = op['modifier']
mod.modifier_strength = 0.5
scene.filepath = r'C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/Fable9B/task5.blend'
blender.save_as()