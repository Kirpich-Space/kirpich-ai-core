

import bpy
import os

# Clear any pre-existing mesh objects but keep cameras and lights
bpy.ops.object.select_all(action='SELECT')
for obj in bpy.context.selected_objects:
    if 'Camera' in obj.name or 'Light' in obj.name:
        obj.select_set(False)
bpy.ops.object.delete(use_select=True)

# Add a cube at the origin with size 2.0
bpy.ops.mesh.primitive_cube_add(size=2.0)
cube = bpy.context.active_object

# Create a red material and assign it to the cube
mat = bpy.data.materials.new(name='Red')
mat.use_nodes = True
node_tree = mat.node_tree
bsdf_node = node_tree.nodes.get('Principled BSDF')
if bsdf_node is None:
    bsdf_node = node_tree.nodes.new('ShaderNodeBsdfPrincipled')

output_node = node_tree.nodes.get('Material Output')
if output_node is None:
    output_node = node_tree.nodes.new('ShaderNodeOutputWorld')

# Ensure BSDF is linked to the output node
existing_link = next((l for l in node_tree.links.values() if l.from_socket.node == bsdf_node and l.to_socket.node == output_node), None)
if existing_link is None:
    node_tree.links.new((bsdf_node.outputs['BSDF'], (output_node.inputs['Surface'],)))

# Set the base color to red
base_color = bsdf_node.inputs.get('Base Color')
if base_color is None:
    bsdf_node['color'] = (1.0, 0.0, 0.0)
else:
    base_color.default_value = (1.0, 0.0, 0.0)

cube.data.materials.append(mat)

# Render settings
bpy.context.scene.render.resolution_x = 320
bpy.context.scene.render.resolution_y = 240

output_path = r'C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.7_en_r1/task2.png'
os.makedirs(os.path.dirname(output_path), exist_ok=True)
bpy.context.scene.render.filepath = output_path

# Render the scene to a PNG image
bpy.ops.render.render(write_still=True)